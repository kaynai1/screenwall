package com.screenwall.android.session

import android.content.Context
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
import com.screenwall.android.net.P2pClient
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
import okhttp3.OkHttpClient
import org.json.JSONArray
import org.json.JSONObject
import org.webrtc.VideoTrack
import java.util.concurrent.atomic.AtomicLong

enum class Phase { Connecting, Waiting, Streaming, Error }

enum class InputMode { Absolute, Trackpad }

/** 当前会话走哪条链路。 */
enum class Link { Relay, P2p }

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
    val link: Link = Link.Relay,
)

data class DisplayInfo(
    val index: Int,
    val width: Int,
    val height: Int,
    val primary: Boolean,
)

/** 一档自动画质：用户手选的档位是天花板，链路差时在这里往下走。 */
private data class AutoTune(
    var baseQuality: Int,
    var baseWidth: Int,
    var quality: Int,
    var width: Int,
    var slow: Int = 0,
    var fine: Int = 0,
    var warmup: Int = 2,
    var announced: Boolean = false,
)

/**
 * 一个控制会话的全部状态：连接（优先 WebRTC 直连、失败回退中继）、订阅、画面、
 * 声音、输入与统计。
 *
 * UI 只读 StateFlow，所有协议消息都从 [ViewerClient] 的监听里进来。
 */
class ControlController(
    private val context: Context,
    private val http: OkHttpClient,
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
    private var subRetry: Job? = null

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
    /** P2P 的视频轨道；界面上的 SurfaceViewRenderer 直接挂这支轨道。 */
    val p2pVideoTrack = MutableStateFlow<VideoTrack?>(null)
    val link = MutableStateFlow(Link.Relay)

    private var p2p: P2pClient? = null
    private var subId = 0

    @Volatile
    private var audioId = 0
    private var pcmFallbackTried = false
    private val bytesIn = AtomicLong(0)
    private var lastRtt = 0L
    private var started = false
    private var disposed = false

    // 自适应：只给被控端提建议，不覆盖用户手选的档位。
    private var auto = AutoTune(
        baseQuality = options.value.quality,
        baseWidth = options.value.maxWidth,
        quality = options.value.quality,
        width = options.value.maxWidth,
    )
    private var deliveredKbps = 0
    private var minAgeMs = Long.MAX_VALUE
    private var maxDelayMs = 0L
    private var seqMark: Int? = null
    private var lastSeq: Int? = null
    private var receivedInWindow = 0
    private var capKbps = 0
    private var capUntil = 0L

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

        override fun onWebrtcSignal(message: JSONObject) {
            p2p?.handleSignal(message)
        }

        override fun onP2pLost(reason: String) {
            scope.launch {
                if (disposed) return@launch
                fallbackToRelay("直连断开，已切回中转")
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
        connectFlow()
        startLoops()
    }

    /** 会话内“重试”：拆掉旧订阅，重新走一遍直连 → 中继。 */
    fun retry() {
        if (disposed) return
        errorText.value = ""
        phase.value = Phase.Connecting
        subId = 0
        p2p?.dispose()
        p2p = null
        p2pVideoTrack.value = null
        link.value = Link.Relay
        if (viewer.session != 0) viewer.closeSession()
        mjpeg.clear()
        h264.reset()
        videoDims.value = null
        isH264.value = false
        viewer.listener = listener
        connectFlow()
    }

    fun dispose() {
        if (disposed) return
        disposed = true
        loops?.cancel()
        loops = null
        subRetry?.cancel()
        subRetry = null
        viewer.sendSession(JSONObject().put("t", "input").put("events", JSONArray().put(InputEvents.releaseAll())))
        val current = subId
        if (current != 0) viewer.sendSession(JSONObject().put("t", "video.unsub").put("id", current))
        val audioCurrent = audioId
        if (audioCurrent != 0) viewer.sendSession(JSONObject().put("t", "audio.unsub").put("id", audioCurrent))
        subId = 0
        audioId = 0
        batcher.clear()
        p2p?.dispose()
        p2p = null
        p2pVideoTrack.value = null
        viewer.closeSession()
        viewer.listener = null
        audio.close()
        h264.stop()
        // 先取画面再停解码器：缩略图只对 MJPEG 有效，H.264 / P2P 走 Surface 取不到帧。
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

    // ---------------------------------------------------------------- 连接

    private fun connectFlow() {
        scope.launch {
            try {
                if (!viewer.isOpen) viewer.connect(account.serverUrl, account.token)
                // 识别码会话的令牌不属于目标设备所在的账号，服务器不会为它转发
                // WebRTC 信令（offer 会被静默丢掉，白等 4 秒），直接走中继。
                val supportsP2p = !account.quick && (device.webrtc || viewer.lastDevices
                    .firstOrNull { it.optString("id") == device.id }
                    ?.optBoolean("webrtc", false) == true)
                var session = 0
                if (supportsP2p) {
                    phase.value = Phase.Connecting
                    val client = P2pClient(
                        context = context,
                        http = http,
                        viewer = viewer,
                        onVideoTrack = { track -> p2pVideoTrack.value = track },
                    )
                    p2p = client
                    // WebRTC 工厂初始化比较重，别占主线程。
                    session = kotlinx.coroutines.withContext(Dispatchers.Default) {
                        client.connect(
                            serverUrl = account.serverUrl,
                            token = account.token,
                            deviceId = device.id,
                            viewerName = AppInfo.CLIENT_NAME,
                        )
                    } ?: 0
                    if (session <= 0) {
                        client.dispose()
                        if (p2p === client) p2p = null
                    }
                }
                if (session <= 0) {
                    viewer.open(device.id, AppInfo.CLIENT_NAME)
                }
                link.value = if (session > 0) Link.P2p else Link.Relay
                phase.value = Phase.Waiting
                sendHello()
            } catch (error: Throwable) {
                fail(openErrorMessage(error))
            }
        }
    }

    /**
     * 直连不可用（编不出 H.264、链路断了、被控端拒绝）：关掉直连，
     * 在同一个账号连接上重新开一条中继会话。
     */
    private fun fallbackToRelay(noticeText: String) {
        if (disposed) return
        notice(noticeText)
        p2p?.dispose()
        p2p = null
        p2pVideoTrack.value = null
        link.value = Link.Relay
        subId = 0
        audioId = 0
        audioOn.value = false
        audioStatus.value = "关闭"
        audio.close()
        mjpeg.clear()
        h264.reset()
        isH264.value = false
        phase.value = Phase.Connecting
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

    // ---------------------------------------------------------------- 订阅

    fun updateOptions(next: StreamOptions) {
        options.value = next
        prefs.saveStreamOptions(next)
        auto = AutoTune(
            baseQuality = next.quality,
            baseWidth = next.maxWidth,
            quality = next.quality,
            width = next.maxWidth,
        )
        applyStream(next)
    }

    fun setInputMode(mode: InputMode) {
        inputMode.value = mode
        prefs.inputMode = mode
    }

    fun selectDisplay(index: Int) {
        if (displayIndex.value == index) return
        displayIndex.value = index
        applyStream(options.value)
    }

    /** 换一档订阅：先退旧订阅，再按当前选项重新订阅。 */
    private fun applyStream(next: StreamOptions) {
        options.value = next
        val current = subId
        if (current != 0) {
            viewer.sendSession(JSONObject().put("t", "video.unsub").put("id", current))
            subId = 0
        }
        if (link.value == Link.Relay) {
            mjpeg.clear()
            h264.reset()
            isH264.value = false
            videoDims.value = null
        }
        sendSubscribe(next)
    }

    private fun sendSubscribe(next: StreamOptions) {
        // P2P 的媒体轨道只打包 H.264；中继上则按用户偏好，编不出来再回 MJPEG。
        val h264 = link.value == Link.P2p || next.preferH264
        val capped = System.currentTimeMillis() < capUntil && capKbps > 0
        val message = JSONObject()
            .put("t", "video.sub")
            .put("stream", "focus")
            .put("display", displayIndex.value)
            .put("fps", next.fps)
            .put("maxWidth", next.maxWidth)
            .put("quality", next.quality)
            .put("codec", if (h264) "h264" else "mjpeg")
            .put("cursor", false)
        if (capped) message.put("maxBitrate", capKbps.coerceIn(200, 12_000))
        viewer.sendSession(message)
        // 慢链路上 video.sub.ok 可能回不来：等不到就重发一次，别停在首帧。
        subRetry?.cancel()
        subRetry = scope.launch {
            delay(5_000)
            if (!disposed && subId == 0 && phase.value != Phase.Error) {
                sendSubscribe(options.value)
            }
        }
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
                    link = link.value,
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
                if (link.value == Link.P2p) {
                    // 直连的声音由 WebRTC 的 Opus 轨道直接播，不走本地 PCM 播放器。
                    audioId = id
                    audioOn.value = true
                    audioStatus.value = "直连 · Opus · $sampleRate Hz"
                    p2p?.setAudioEnabled(true)
                    return
                }
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
                        if (link.value == Link.P2p) {
                            // 直连轨道只有 H.264，编不出来就只能回中继走 MJPEG。
                            options.value = options.value.copy(preferH264 = false)
                            fallbackToRelay("这台设备不支持 H.264 编码，已切回中转")
                        } else if (options.value.preferH264) {
                            // 只在本会话里降级，不改用户偏好：被控端升级后下次会话还能再试。
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
            if (!viewer.isP2p) {
                // 帧年龄 = 现在 − 采集时间；减去见过的最小年龄就是它在链路上排了多久。
                val age = System.currentTimeMillis() - frame.tsUs / 1000
                if (age < minAgeMs) {
                    minAgeMs = age
                } else if (age - minAgeMs > maxDelayMs) {
                    maxDelayMs = age - minAgeMs
                }
                lastSeq = frame.seq
                receivedInWindow += 1
                // 收到即确认：服务器据此把在途帧压到 1–8 帧，慢链路的延迟不会越积越大。
                viewer.sendSession(JSONObject().put("t", "video.ack").put("seq", frame.seq))
            }
            when (frame.codec) {
                Protocol.CODEC_JPEG -> mjpeg.push(frame.payload)
                Protocol.CODEC_H264 -> h264.push(frame)
            }
            return
        }
        if (frame.type == Protocol.MEDIA_AUDIO) {
            if (link.value == Link.P2p) return
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
            sendSubscribe(next)
        }
    }

    // ---------------------------------------------------------------- 循环

    private fun startLoops() {
        loops?.cancel()
        loops = scope.launch {
            launch {
                while (true) {
                    delay(1_000)
                    val p2pStats = if (link.value == Link.P2p) p2p?.stats else null
                    if (p2pStats != null) {
                        deliveredKbps = p2pStats.kbps
                        if (p2pStats.width > 0 && p2pStats.height > 0) {
                            videoDims.value = p2pStats.width to p2pStats.height
                        }
                        stats.value = SessionStats(
                            fps = p2pStats.fps.toDouble(),
                            kbps = p2pStats.kbps,
                            rttMs = p2pStats.rttMs,
                            width = p2pStats.width,
                            height = p2pStats.height,
                            codec = "H.264",
                            link = Link.P2p,
                        )
                    } else {
                        val mjpegFps = mjpeg.sample().first
                        val h264Fps = if (isH264.value) h264.sample() else 0.0
                        val kbps = (bytesIn.getAndSet(0) * 8 / 1000).toInt()
                        val dims = videoDims.value
                        deliveredKbps = kbps
                        stats.value = stats.value.copy(
                            fps = if (isH264.value) h264Fps else mjpegFps,
                            kbps = kbps,
                            rttMs = lastRtt,
                            width = dims?.first ?: stats.value.width,
                            height = dims?.second ?: stats.value.height,
                            link = Link.Relay,
                        )
                        val mark = seqMark
                        val produced = lastSeq?.let { last ->
                            if (mark == null) 0L else ((last.toLong() - mark) and 0xFFFF_FFFFL)
                        } ?: 0L
                        val ratio = if (produced >= AUTO_RATIO_MIN_FRAMES) {
                            (receivedInWindow / produced.toDouble()).coerceAtMost(1.0)
                        } else {
                            null
                        }
                        seqMark = lastSeq
                        receivedInWindow = 0
                        tune(ratio, maxDelayMs, lastRtt)
                        maxDelayMs = 0
                    }
                }
            }
            launch {
                while (true) {
                    // 延迟也是降档信号，2 秒一次让 RTT 跟得上。
                    delay(2_000)
                    viewer.sendSession(JSONObject().put("t", "ping").put("ts", System.currentTimeMillis()))
                }
            }
        }
    }

    /**
     * 每秒看一次排队时长、到手比例和往返延迟：积压就降档，清爽够久再升回去。
     * 不看帧率——桌面静止时被控端本来就是补发帧，帧率低不代表链路跑不动。
     */
    private fun tune(ratio: Double?, delay: Long, rtt: Long) {
        val target = if (options.value.fps == 0) 60 else options.value.fps
        if (phase.value != Phase.Streaming || subId == 0 || target < 10) return
        if (auto.warmup > 0) {
            auto.warmup -= 1
            return
        }
        if (System.currentTimeMillis() >= capUntil) capKbps = 0
        val starved = ratio != null && ratio < AUTO_RATIO_SLOW
        val slow = starved || delay > AUTO_DELAY_SLOW_MS || rtt > AUTO_RTT_SLOW_MS
        if (slow) {
            // 拥塞时把实测到手码率记成上限，下一档订阅让被控端按这个预算压帧。
            capKbps = maxOf(capKbps, (deliveredKbps * 1.15).toInt())
            capUntil = System.currentTimeMillis() + AUTO_CAP_HOLD_MS
            auto.fine = 0
            auto.slow += 1
            val severe = (ratio != null && ratio < AUTO_RATIO_SEVERE) ||
                delay > AUTO_DELAY_SEVERE_MS ||
                rtt > 2_500
            if (!severe && auto.slow < AUTO_SLOW_WINDOWS) return
            auto.slow = 0
            val steps = if (severe) 2 else 1
            var changed = false
            repeat(steps) { if (stepDown()) changed = true }
            if (!changed) return
            auto.warmup = if (severe) 2 else 3
            if (!auto.announced) {
                auto.announced = true
                notice("网络较慢，已自动降低画质")
            }
            applyStream(options.value.copy(quality = auto.quality, maxWidth = auto.width))
            return
        }
        if ((ratio == null || ratio > AUTO_RATIO_FINE) &&
            delay < AUTO_DELAY_FINE_MS &&
            rtt < AUTO_RTT_FINE_MS
        ) {
            auto.slow = 0
            auto.fine += 1
            if (auto.fine < AUTO_FINE_WINDOWS) return
            auto.fine = 0
            if (!stepUp()) return
            auto.warmup = 4
            applyStream(options.value.copy(quality = auto.quality, maxWidth = auto.width))
            return
        }
        auto.slow = 0
        auto.fine = 0
    }

    /** 降一档：先降分辨率（到 640 宽）再压质量，最后继续往 480/400 降。 */
    private fun stepDown(): Boolean {
        val nextWidth = widthBelow(auto.width)
        if (nextWidth != null && auto.width > 640) {
            auto.width = nextWidth
            return true
        }
        val nextQuality = qualityBelow(auto.quality)
        if (nextQuality != null) {
            auto.quality = nextQuality
            return true
        }
        if (nextWidth != null) {
            auto.width = nextWidth
            return true
        }
        return false
    }

    /** 升一档：先把质量还回去，再恢复分辨率，且不超过用户手选的档位。 */
    private fun stepUp(): Boolean {
        val nextQuality = qualityAbove(auto.quality, auto.baseQuality)
        if (nextQuality != null) {
            auto.quality = nextQuality
            return true
        }
        val nextWidth = widthAbove(auto.width, auto.baseWidth)
        if (nextWidth != null) {
            auto.width = nextWidth
            return true
        }
        return false
    }

    private fun widthBelow(current: Int): Int? =
        AUTO_WIDTH_STEPS.filter { it < current }.maxOrNull()

    private fun widthAbove(current: Int, ceiling: Int): Int? =
        AUTO_WIDTH_STEPS.filter { it > current && it <= ceiling }.minOrNull()

    private fun qualityBelow(current: Int): Int? =
        AUTO_QUALITY_STEPS.filter { it < current }.maxOrNull()

    private fun qualityAbove(current: Int, ceiling: Int): Int? =
        AUTO_QUALITY_STEPS.filter { it > current && it <= ceiling }.minOrNull()

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
        p2p?.setAudioEnabled(false)
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
        if (surface != null && subId != 0 && link.value == Link.Relay) {
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

    private companion object {
        val AUTO_WIDTH_STEPS = listOf(3840, 2560, 1920, 1440, 1280, 960, 720, 640, 480, 400)
        val AUTO_QUALITY_STEPS = listOf(80, 65, 55, 45, 35, 30)
        const val AUTO_SLOW_WINDOWS = 2
        const val AUTO_FINE_WINDOWS = 8
        const val AUTO_RTT_SLOW_MS = 1_500L
        const val AUTO_RTT_FINE_MS = 400L
        const val AUTO_DELAY_SLOW_MS = 500L
        const val AUTO_DELAY_SEVERE_MS = 1_200L
        const val AUTO_DELAY_FINE_MS = 250L
        const val AUTO_RATIO_SEVERE = 0.4
        const val AUTO_RATIO_SLOW = 0.65
        const val AUTO_RATIO_FINE = 0.9
        const val AUTO_RATIO_MIN_FRAMES = 4L
        const val AUTO_CAP_HOLD_MS = 10_000L
    }
}
