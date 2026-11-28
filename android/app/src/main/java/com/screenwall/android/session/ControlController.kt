package com.screenwall.android.session

import android.view.Surface
import com.screenwall.android.Prefs
import com.screenwall.android.audio.AudioPlayer
import com.screenwall.android.data.Account
import com.screenwall.android.data.AppInfo
import com.screenwall.android.data.Device
import com.screenwall.android.data.describeCode
import com.screenwall.android.data.describeThrow
import com.screenwall.android.input.InputBatcher
import com.screenwall.android.input.InputEvents
import com.screenwall.android.net.MediaFrame
import com.screenwall.android.net.OpenError
import com.screenwall.android.net.Protocol
import com.screenwall.android.net.ViewerClient
import com.screenwall.android.video.H264Decoder
import com.screenwall.android.video.MjpegDecoder
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicLong

enum class Phase { Connecting, Waiting, Streaming, Error }

enum class InputMode { Absolute, Trackpad }

data class StreamOptions(
    val maxWidth: Int = 1280,
    val fps: Int = 30,
    val quality: Int = 65,
    val preferH264: Boolean = true,
)

data class SessionStats(
    val fps: Double = 0.0,
    val kbps: Int = 0,
    val rttMs: Long = 0,
    val width: Int = 0,
    val height: Int = 0,
    val codec: String = "",
)

data class DisplayInfo(
    val index: Int,
    val width: Int,
    val height: Int,
    val primary: Boolean,
)

/**
 * 一个控制会话的全部状态：连接、订阅、画面、声音、输入与统计。
 *
 * UI 只读 StateFlow，所有协议消息都从 [ViewerClient] 的监听里进来。
 */
class ControlController(
    private val viewer: ViewerClient,
    private val account: Account,
    private val device: Device,
    private val prefs: Prefs,
) {
    val mjpeg = MjpegDecoder()

    private val h264 = H264Decoder(
        onFrameSize = { width, height ->
            videoDims.value = width to height
            stats.value = stats.value.copy(width = width, height = height)
        },
        onError = { onH264Broken() },
    )
    private val audio = AudioPlayer { message -> notice(message) }

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var loops: Job? = null

    val phase = MutableStateFlow(Phase.Connecting)
    val errorText = MutableStateFlow("")
    val stats = MutableStateFlow(SessionStats())
    val displays = MutableStateFlow<List<DisplayInfo>>(emptyList())
    val options = MutableStateFlow(prefs.streamOptions())
    val inputMode = MutableStateFlow(prefs.inputMode)
    val audioOn = MutableStateFlow(false)
    val audioStatus = MutableStateFlow("关闭")
    val videoDims = MutableStateFlow<Pair<Int, Int>?>(null)
    val isH264 = MutableStateFlow(false)
    val displayIndex = MutableStateFlow(0)
    val notices = MutableSharedFlow<String>(extraBufferCapacity = 8)

    private var subId = 0
    @Volatile
    private var audioId = 0
    private var pcmFallbackTried = false
    private val bytesIn = AtomicLong(0)
    private var lastRtt = 0L
    private var started = false
    private var disposed = false

    private val batcher = InputBatcher { events ->
        viewer.sendSession(JSONObject().put("t", "input").put("events", events))
    }

    private val listener = object : ViewerClient.Listener {
        override fun onControlClosed(code: Int, msg: String) {
            scope.launch {
                if (disposed) return@launch
                subId = 0
                fail(describeCode(code, msg))
            }
        }

        override fun onSessionMessage(message: JSONObject) {
            scope.launch { if (!disposed) handleMessage(message) }
        }

        override fun onFrame(frame: MediaFrame) {
            handleFrame(frame)
        }

        override fun onSocketClosed(reason: String) {
            scope.launch {
                if (disposed) return@launch
                fail("连接已断开：$reason")
            }
        }
    }

    fun start() {
        if (started) return
        started = true
        disposed = false
        prefs.addRecent(device.id, device.name, device.os)
        viewer.listener = listener
        mjpeg.start()
        h264.start()
        scope.launch {
            try {
                if (!viewer.isOpen) viewer.connect(account.serverUrl, account.token)
                viewer.open(device.id, AppInfo.CLIENT_NAME)
                phase.value = Phase.Waiting
                sendHello()
            } catch (error: Throwable) {
                fail(openErrorMessage(error))
            }
        }
        startLoops()
    }

    /** 会话内“重试”：拆掉旧订阅，按当前选项重新开一次。 */
    fun retry() {
        if (disposed) return
        errorText.value = ""
        phase.value = Phase.Connecting
        subId = 0
        if (viewer.session != 0) viewer.closeSession()
        mjpeg.clear()
        h264.reset()
        videoDims.value = null
        isH264.value = false
        viewer.listener = listener
        scope.launch {
            try {
                if (!viewer.isOpen) viewer.connect(account.serverUrl, account.token)
                viewer.open(device.id, AppInfo.CLIENT_NAME)
                phase.value = Phase.Waiting
                sendHello()
            } catch (error: Throwable) {
                fail(openErrorMessage(error))
            }
        }
    }

    fun dispose() {
        if (disposed) return
        disposed = true
        loops?.cancel()
        loops = null
        viewer.sendSession(JSONObject().put("t", "input").put("events", JSONArray().put(InputEvents.releaseAll())))
        val current = subId
        if (current != 0) viewer.sendSession(JSONObject().put("t", "video.unsub").put("id", current))
        val audioCurrent = audioId
        if (audioCurrent != 0) viewer.sendSession(JSONObject().put("t", "audio.unsub").put("id", audioCurrent))
        subId = 0
        audioId = 0
        batcher.clear()
        viewer.closeSession()
        viewer.listener = null
        audio.close()
        h264.stop()
        // 先取画面再停解码器：缩略图只对 MJPEG 有效，H.264 走 Surface 取不到帧。
        runCatching { savePreviewIfEnabled() }
        mjpeg.stop()
        scope.cancel()
    }

    private fun savePreviewIfEnabled() {
        if (!prefs.savePreview) return
        val bitmap = mjpeg.snapshot() ?: return
        try {
            prefs.savePreview(device.id, bitmap)
        } finally {
            bitmap.recycle()
        }
    }

    // ---------------------------------------------------------------- 订阅

    fun updateOptions(next: StreamOptions) {
        options.value = next
        prefs.saveStreamOptions(next)
        resubscribe(next)
    }

    fun setInputMode(mode: InputMode) {
        inputMode.value = mode
        prefs.inputMode = mode
    }

    fun selectDisplay(index: Int) {
        if (displayIndex.value == index) return
        displayIndex.value = index
        resubscribe(options.value)
    }

    private fun resubscribe(next: StreamOptions) {
        val current = subId
        if (current != 0) {
            viewer.sendSession(JSONObject().put("t", "video.unsub").put("id", current))
            subId = 0
        }
        mjpeg.clear()
        h264.reset()
        isH264.value = false
        videoDims.value = null
        phase.value = Phase.Waiting
        sendSubscribe(next)
    }

    private fun sendSubscribe(next: StreamOptions) {
        val message = JSONObject()
            .put("t", "video.sub")
            .put("stream", "focus")
            .put("display", displayIndex.value)
            .put("fps", next.fps)
            .put("maxWidth", next.maxWidth)
            .put("quality", next.quality)
            .put("codec", if (next.preferH264) "h264" else "mjpeg")
            .put("cursor", false)
        viewer.sendSession(message)
    }

    private fun sendHello() {
        val hello = JSONObject()
            .put("t", "hello")
            .put("proto", 1)
            .put("token", "")
            .put("client", JSONObject().put("name", AppInfo.CLIENT_NAME).put("version", AppInfo.VERSION))
            // 空数组按 v1 基线处理，可选能力一条都不虚报。
            .put("caps", JSONArray())
        viewer.sendSession(hello)
    }

    private fun handleMessage(message: JSONObject) {
        when (message.optString("t")) {
            "welcome" -> sendSubscribe(options.value)
            "display.list" -> {
                val array = message.optJSONArray("displays") ?: JSONArray()
                val list = (0 until array.length()).mapNotNull { index ->
                    val item = array.optJSONObject(index) ?: return@mapNotNull null
                    DisplayInfo(
                        index = item.optInt("index", index),
                        width = item.optInt("w"),
                        height = item.optInt("h"),
                        primary = item.optBoolean("primary"),
                    )
                }
                displays.value = list
                if (list.isNotEmpty() && list.none { it.index == displayIndex.value }) {
                    displayIndex.value = list.firstOrNull { it.primary }?.index ?: list.first().index
                }
            }
            "video.sub.ok" -> {
                subId = message.optInt("id", 0)
                val actual = message.optJSONObject("actual")
                val codec = actual?.optString("codec").orEmpty()
                val width = actual?.optInt("w") ?: 0
                val height = actual?.optInt("h") ?: 0
                isH264.value = codec == "h264"
                if (width > 0 && height > 0) {
                    videoDims.value = width to height
                    h264.setExpectedSize(width, height)
                }
                stats.value = stats.value.copy(
                    width = width,
                    height = height,
                    codec = if (codec == "h264") "H.264" else "MJPEG",
                )
                phase.value = Phase.Streaming
                errorText.value = ""
            }
            "pong" -> {
                val sent = message.optLong("ts", 0)
                if (sent > 0) lastRtt = (System.currentTimeMillis() - sent).coerceAtLeast(0)
            }
            "audio.sub.ok" -> {
                val actual = message.optJSONObject("actual")
                val codec = actual?.optString("codec").orEmpty()
                val sampleRate = actual?.optInt("sampleRate", 48_000) ?: 48_000
                val channels = actual?.optInt("channels", 2) ?: 2
                val id = message.optInt("id", 0)
                if (audio.configure(codec, sampleRate, channels)) {
                    audioId = id
                    audioOn.value = true
                    audioStatus.value = when (codec) {
                        "opus" -> "Opus · $sampleRate Hz"
                        else -> "PCM16 · $sampleRate Hz"
                    }
                } else {
                    viewer.sendSession(JSONObject().put("t", "audio.unsub").put("id", id))
                    if (!pcmFallbackTried) {
                        pcmFallbackTried = true
                        viewer.sendSession(JSONObject().put("t", "audio.sub").put("codec", "pcm16"))
                    } else {
                        audioOn.value = false
                        audioStatus.value = "这台设备解不了远端声音"
                        notice("声音解码不可用")
                    }
                }
            }
            "audio.unsub.ok" -> Unit
            "sas.ok" -> {
                val method = message.optString("method")
                notice(if (method == "keys") "已降级为普通 Ctrl+Alt+Del 按键" else "已发送 Ctrl+Alt+Del")
            }
            "input.status" -> {
                if (message.optBoolean("uipi")) notice("远端窗口权限更高，键鼠可能被系统拦截")
            }
            "error" -> {
                val code = message.optInt("code", 0)
                val msg = message.optString("msg")
                when {
                    code == 4104 && subId == 0 -> {
                        if (options.value.preferH264) {
                            // 只在本会话里降级，不改用户偏好：被控端升级后下次会话还能再试 H.264。
                            val next = options.value.copy(preferH264 = false)
                            options.value = next
                            notice("H.264 不可用，已切回 MJPEG")
                            sendSubscribe(next)
                        } else {
                            fail(describeCode(code, msg))
                        }
                    }
                    code == 4110 -> {
                        audioOn.value = false
                        audioStatus.value = "被控端无法采集声音"
                        notice(describeCode(code, msg))
                    }
                    else -> notice(describeCode(code, msg))
                }
            }
            else -> Unit
        }
    }

    private fun handleFrame(frame: MediaFrame) {
        if (frame.type == Protocol.MEDIA_VIDEO) {
            bytesIn.addAndGet(frame.payload.size.toLong())
            when (frame.codec) {
                Protocol.CODEC_JPEG -> mjpeg.push(frame.payload)
                Protocol.CODEC_H264 -> h264.push(frame)
            }
            return
        }
        if (frame.type == Protocol.MEDIA_AUDIO) {
            val current = audioId
            if (current != 0 && frame.streamId == current) {
                audio.push(frame.codec, frame.payload)
            }
        }
    }

    private fun onH264Broken() {
        scope.launch {
            if (disposed) return@launch
            notice("H.264 解码失败，已切回 MJPEG")
            // 同样只影响本次会话，下次还是先试 H.264。
            val next = options.value.copy(preferH264 = false)
            options.value = next
            resubscribe(next)
        }
    }

    // ---------------------------------------------------------------- 循环

    private fun startLoops() {
        loops?.cancel()
        loops = scope.launch {
            launch {
                while (true) {
                    delay(1_000)
                    val mjpegFps = mjpeg.sample().first
                    val h264Fps = if (isH264.value) h264.sample() else 0.0
                    val kbps = (bytesIn.getAndSet(0) * 8 / 1000).toInt()
                    val dims = videoDims.value
                    stats.value = stats.value.copy(
                        fps = if (isH264.value) h264Fps else mjpegFps,
                        kbps = kbps,
                        rttMs = lastRtt,
                        width = dims?.first ?: stats.value.width,
                        height = dims?.second ?: stats.value.height,
                    )
                }
            }
            launch {
                while (true) {
                    delay(5_000)
                    viewer.sendSession(JSONObject().put("t", "ping").put("ts", System.currentTimeMillis()))
                }
            }
        }
    }

    // ---------------------------------------------------------------- 声音

    fun toggleAudio() {
        if (audioId != 0 || audioOn.value) {
            stopAudio()
            return
        }
        pcmFallbackTried = false
        audioStatus.value = "正在开启…"
        viewer.sendSession(JSONObject().put("t", "audio.sub").put("codec", "opus"))
    }

    private fun stopAudio() {
        val current = audioId
        audioId = 0
        if (current != 0) viewer.sendSession(JSONObject().put("t", "audio.unsub").put("id", current))
        audio.close()
        audioOn.value = false
        audioStatus.value = "关闭"
    }

    // ---------------------------------------------------------------- 输入

    /** MJPEG 实际解码出的大小可能与 `video.sub.ok` 相差几像素，以画面为准。 */
    fun onMjpegFrame(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        val current = videoDims.value
        if (current == null || current.first != width || current.second != height) {
            videoDims.value = width to height
        }
    }

    fun markH264Surface(surface: Surface?, width: Int, height: Int) {
        h264.setSurface(surface, width, height)
        if (surface != null && subId != 0) {
            viewer.sendSession(JSONObject().put("t", "video.keyframe").put("id", subId))
        }
    }

    fun inputMoveAbs(x: Float, y: Float) = batcher.push(InputEvents.moveAbs(x, y))

    fun inputMoveRel(dx: Int, dy: Int) = batcher.push(InputEvents.moveRel(dx, dy))

    fun inputButton(name: String, down: Boolean, x: Float? = null, y: Float? = null) =
        batcher.push(InputEvents.button(name, down, x, y))

    fun inputWheel(dx: Int, dy: Int) = batcher.push(InputEvents.wheel(dx, dy))

    fun flushInput() = batcher.flush()

    fun sendText(text: String) {
        if (text.isEmpty()) return
        batcher.push(InputEvents.text(text))
        batcher.flush()
    }

    fun sendKey(scan: Int, ext: Boolean = false) {
        batcher.push(InputEvents.key(scan, down = true, ext = ext))
        batcher.push(InputEvents.key(scan, down = false, ext = ext))
        batcher.flush()
    }

    fun sendBackspace() = sendKey(0x0e)

    fun sendBackspaces(count: Int) {
        if (count <= 0) return
        repeat(count) {
            batcher.push(InputEvents.key(0x0e, down = true))
            batcher.push(InputEvents.key(0x0e, down = false))
        }
        batcher.flush()
    }

    fun sendEnter() = sendKey(0x1c)

    fun sendSecureAttention() {
        viewer.sendSession(JSONObject().put("t", "sas").put("asUser", false))
    }

    private fun fail(message: String) {
        phase.value = Phase.Error
        errorText.value = message
        subId = 0
        audioOn.value = false
        audioId = 0
        audio.close()
    }

    private fun notice(message: String) {
        notices.tryEmit(message)
    }

    private fun openErrorMessage(error: Throwable): String = when (error) {
        is OpenError -> describeCode(error.code, error.msg)
        else -> describeThrow(error)
    }
}
