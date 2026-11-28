package com.screenwall.android.net

import android.content.Context
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import org.webrtc.AudioTrack
import org.webrtc.DataChannel
import org.webrtc.DefaultVideoDecoderFactory
import org.webrtc.EglBase
import org.webrtc.IceCandidate
import org.webrtc.MediaConstraints
import org.webrtc.MediaStream
import org.webrtc.MediaStreamTrack
import org.webrtc.PeerConnection
import org.webrtc.PeerConnectionFactory
import org.webrtc.RtpReceiver
import org.webrtc.RtpTransceiver
import org.webrtc.SdpObserver
import org.webrtc.SessionDescription
import org.webrtc.VideoTrack
import org.webrtc.audio.JavaAudioDeviceModule
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicInteger
import kotlin.coroutines.resume

/** 直连链路最近一秒的统计（`PeerConnection.getStats()`）。 */
data class P2pStats(
    val fps: Int = 0,
    val kbps: Int = 0,
    val rttMs: Long = 0,
    val width: Int = 0,
    val height: Int = 0,
)

/**
 * 进程内共享的 WebRTC 引擎：工厂和 EGL 上下文建一次，所有直连会话复用。
 *
 * 不销毁是刻意的——被控端只在有会话时才推流，空闲时这里没有解码器在跑；
 * 每次会话重建工厂要一两秒，反而让「先试直连、失败回中继」变慢。
 */
object P2pEngine {
    private var initialized = false
    private var factory: PeerConnectionFactory? = null
    private var egl: EglBase? = null

    @Synchronized
    fun factory(context: Context): PeerConnectionFactory {
        factory?.let { return it }
        if (!initialized) {
            PeerConnectionFactory.initialize(
                PeerConnectionFactory.InitializationOptions.builder(context.applicationContext)
                    .setEnableInternalTracer(false)
                    .createInitializationOptions(),
            )
            initialized = true
        }
        val audio = JavaAudioDeviceModule.builder(context.applicationContext)
            // 只放远端声音，不采麦克风：硬件 AEC/NS 用不上，关掉省一次录音权限探测。
            .setUseHardwareAcousticEchoCanceler(false)
            .setUseHardwareNoiseSuppressor(false)
            .createAudioDeviceModule()
        val created = PeerConnectionFactory.builder()
            .setAudioDeviceModule(audio)
            .setVideoDecoderFactory(DefaultVideoDecoderFactory(eglBase().eglBaseContext))
            .createPeerConnectionFactory()
        factory = created
        return created
    }

    @Synchronized
    fun eglBase(): EglBase {
        egl?.let { return it }
        val created = EglBase.create()
        egl = created
        return created
    }
}

/**
 * 与网页端同一套 WebRTC 直连：服务器只转发 `t:"webrtc"` 信令，协商完成后
 * 画面/声音走 H.264 / Opus 轨道，打开、输入、视频订阅走 `ctrl` DataChannel。
 *
 * 连不上时返回 null，调用方回退到 `/ws/view` 中继，用户只会多等一两秒。
 */
class P2pClient(
    private val context: Context,
    private val http: OkHttpClient,
    private val viewer: ViewerClient,
    private val onVideoTrack: (VideoTrack?) -> Unit,
) : ChannelTransport {

    @Volatile
    var stats: P2pStats = P2pStats()
        private set

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val lock = Any()
    private val iceQueue = mutableListOf<IceCandidate>()

    private var pc: PeerConnection? = null
    private var dc: DataChannel? = null
    private var remoteAudio: AudioTrack? = null
    private var remoteVideo: VideoTrack? = null
    private var audioWanted = false
    private var remoteSet = false
    private var request = 0
    private var openRequest = 0
    private var pendingOpen: CompletableDeferred<Int>? = null
    private var deviceId = ""
    private var viewerName = ""

    @Volatile
    private var adopted = false

    @Volatile
    private var closed = false

    private var statsBytes = 0L
    private var statsAt = 0L
    private var lastRtt = 0L
    private var lastWidth = 0
    private var lastHeight = 0

    /** 打开直连并完成 `open` 握手；成功返回会话号，失败返回 null。 */
    suspend fun connect(
        serverUrl: String,
        token: String,
        deviceId: String,
        viewerName: String,
    ): Int? {
        this.deviceId = deviceId
        this.viewerName = viewerName
        val iceServers = fetchIceServers(serverUrl, token) ?: return null
        val factory = P2pEngine.factory(context)
        val config = PeerConnection.RTCConfiguration(iceServers).apply {
            sdpSemantics = PeerConnection.SdpSemantics.UNIFIED_PLAN
            bundlePolicy = PeerConnection.BundlePolicy.MAXBUNDLE
            iceTransportsType = PeerConnection.IceTransportsType.ALL
            continualGatheringPolicy = PeerConnection.ContinualGatheringPolicy.GATHER_CONTINUALLY
        }
        val peer = factory.createPeerConnection(config, observer) ?: return null
        pc = peer
        // 两条只收的 m-line：被控端按它们挂 H.264 / Opus 轨道。
        peer.addTransceiver(
            MediaStreamTrack.MediaType.MEDIA_TYPE_VIDEO,
            RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.RECV_ONLY),
        )
        peer.addTransceiver(
            MediaStreamTrack.MediaType.MEDIA_TYPE_AUDIO,
            RtpTransceiver.RtpTransceiverInit(RtpTransceiver.RtpTransceiverDirection.RECV_ONLY),
        )
        val channel = peer.createDataChannel("ctrl", DataChannel.Init().apply { ordered = true })
            ?: return failConnect(null)
        dc = channel
        channel.registerObserver(dcObserver)

        val open = CompletableDeferred<Int>()
        pendingOpen = open
        request = SIGNAL_SEQ.incrementAndGet()

        val offerSdp = withTimeoutOrNull(3_000) { createOffer(peer) }
            ?: return failConnect("创建 offer 失败")
        if (!setLocalDescription(peer, SessionDescription(SessionDescription.Type.OFFER, offerSdp))) {
            return failConnect("设置本地 offer 失败")
        }
        val sdp = peer.localDescription?.description.orEmpty()
        if (sdp.isBlank() || !sendSignal("offer", JSONObject().put("sdp", sdp))) {
            return failConnect("发送 offer 失败")
        }

        val session = withTimeoutOrNull(CONNECT_TIMEOUT_MS) { open.await() }
        if (session == null || session <= 0 || closed) {
            return failConnect("直连建立超时")
        }
        adopted = true
        viewer.adoptP2p(this, session)
        startStats()
        return session
    }

    /** 处理服务器经 `/ws/view` 转来的 `t:"webrtc"` 消息。 */
    fun handleSignal(value: JSONObject) {
        if (value.optLong("request", 0) != request.toLong()) return
        when (value.optString("event")) {
            "answer" -> applyAnswer(value.optString("sdp"))
            "ice" -> applyIce(value.optJSONObject("ice"))
            "reject", "bye" -> {
                if (adopted) lost("直连已断开") else failConnect(value.optString("msg", "被控端拒绝了直连"))
            }
        }
    }

    fun setAudioEnabled(on: Boolean) {
        audioWanted = on
        runCatching { remoteAudio?.setEnabled(on) }
    }

    /** 界面离开或会话结束时调用；不再回调 onLost。 */
    fun dispose() {
        if (closed) return
        closed = true
        if (adopted) runCatching { viewer.closeSession() }
        sendSignal("bye", JSONObject())
        adopted = false
        teardown()
    }

    override val isOpen: Boolean
        get() = dc?.state() == DataChannel.State.OPEN

    override fun send(bytes: ByteArray): Boolean {
        val channel = dc ?: return false
        return channel.send(DataChannel.Buffer(ByteBuffer.wrap(bytes), true))
    }

    override fun sendText(text: String): Boolean {
        val channel = dc ?: return false
        return channel.send(DataChannel.Buffer(ByteBuffer.wrap(text.toByteArray()), false))
    }

    override fun close() = dispose()

    // ---------------------------------------------------------------- 信令

    private fun sendSignal(event: String, extra: JSONObject): Boolean {
        if (request == 0) return false
        val message = JSONObject()
            .put("t", "webrtc")
            .put("request", request)
            .put("deviceId", deviceId)
            .put("viewer", viewerName)
            .put("event", event)
        val keys = extra.keys()
        while (keys.hasNext()) {
            val key = keys.next()
            message.put(key, extra.get(key))
        }
        return viewer.sendText(message)
    }

    private suspend fun fetchIceServers(serverUrl: String, token: String): List<PeerConnection.IceServer>? =
        suspendCancellableCoroutine { continuation ->
            val request = Request.Builder()
                .url("${serverUrl.trimEnd('/')}/api/v1/p2p/ice")
                .header("authorization", "Bearer $token")
                .post(ByteArray(0).toRequestBody("application/json".toMediaType()))
                .build()
            http.newCall(request).enqueue(object : okhttp3.Callback {
                override fun onFailure(call: okhttp3.Call, e: java.io.IOException) {
                    if (continuation.isActive) continuation.resume(null)
                }

                override fun onResponse(call: okhttp3.Call, response: okhttp3.Response) {
                    val result = response.use {
                        if (!it.isSuccessful) {
                            null
                        } else {
                            val text = it.body?.string().orEmpty()
                            parseIceServers(text)
                        }
                    }
                    if (continuation.isActive) continuation.resume(result)
                }
            })
        }

    private fun parseIceServers(text: String): List<PeerConnection.IceServer>? {
        val array = runCatching { JSONObject(text).optJSONArray("iceServers") }
            .getOrNull() as? JSONArray ?: return null
        val servers = mutableListOf<PeerConnection.IceServer>()
        for (index in 0 until array.length()) {
            val item = array.optJSONObject(index) ?: continue
            val urls = mutableListOf<String>()
            val raw = item.opt("urls")
            if (raw is JSONArray) {
                for (i in 0 until raw.length()) raw.optString(i).takeIf { it.isNotBlank() }?.let(urls::add)
            } else if (raw is String && raw.isNotBlank()) {
                urls.add(raw)
            }
            if (urls.isEmpty()) continue
            val builder = PeerConnection.IceServer.builder(urls)
            item.optString("username").takeIf { it.isNotBlank() }?.let(builder::setUsername)
            item.optString("credential").takeIf { it.isNotBlank() }?.let(builder::setPassword)
            runCatching { builder.createIceServer() }.getOrNull()?.let(servers::add)
        }
        return servers.takeIf { it.isNotEmpty() }
    }

    private fun applyAnswer(sdp: String) {
        if (sdp.isBlank()) return
        val peer = pc ?: return
        peer.setRemoteDescription(
            object : SdpObserver {
                override fun onCreateSuccess(desc: SessionDescription?) = Unit
                override fun onSetSuccess() {
                    remoteSet = true
                    flushIce()
                }

                override fun onCreateFailure(error: String?) = Unit
                override fun onSetFailure(error: String?) {
                    if (adopted) lost("直连协商失败") else failConnect("直连协商失败")
                }
            },
            SessionDescription(SessionDescription.Type.ANSWER, sdp),
        )
    }

    private fun applyIce(value: JSONObject?) {
        val candidate = value?.optString("candidate").orEmpty().trim()
        if (!candidate.startsWith("candidate:")) return
        val ice = IceCandidate(
            value?.optString("sdpMid").orEmpty(),
            value?.optInt("sdpMLineIndex", 0) ?: 0,
            candidate,
        )
        synchronized(lock) {
            if (!remoteSet) {
                if (iceQueue.size < 64) iceQueue.add(ice)
                return
            }
        }
        runCatching { pc?.addIceCandidate(ice) }
    }

    private fun flushIce() {
        val queued = synchronized(lock) {
            val copy = iceQueue.toList()
            iceQueue.clear()
            copy
        }
        queued.forEach { runCatching { pc?.addIceCandidate(it) } }
    }

    private suspend fun createOffer(peer: PeerConnection): String? =
        suspendCancellableCoroutine { continuation ->
            peer.createOffer(
                object : SdpObserver {
                    override fun onCreateSuccess(desc: SessionDescription?) {
                        if (continuation.isActive) continuation.resume(desc?.description)
                    }

                    override fun onSetSuccess() = Unit
                    override fun onCreateFailure(error: String?) {
                        if (continuation.isActive) continuation.resume(null)
                    }

                    override fun onSetFailure(error: String?) = Unit
                },
                MediaConstraints(),
            )
        }

    private suspend fun setLocalDescription(
        peer: PeerConnection,
        description: SessionDescription,
    ): Boolean = suspendCancellableCoroutine { continuation ->
        peer.setLocalDescription(
            object : SdpObserver {
                override fun onCreateSuccess(desc: SessionDescription?) = Unit
                override fun onSetSuccess() {
                    if (continuation.isActive) continuation.resume(true)
                }

                override fun onCreateFailure(error: String?) = Unit
                override fun onSetFailure(error: String?) {
                    if (continuation.isActive) continuation.resume(false)
                }
            },
            description,
        )
    }

    // ---------------------------------------------------------------- PeerConnection

    private val observer = object : PeerConnection.Observer {
        override fun onSignalingChange(state: PeerConnection.SignalingState?) = Unit

        override fun onIceConnectionChange(state: PeerConnection.IceConnectionState?) {
            when (state) {
                PeerConnection.IceConnectionState.FAILED -> {
                    if (adopted) lost("直连已断开") else failConnect("直连已断开")
                }

                PeerConnection.IceConnectionState.DISCONNECTED -> {
                    // 先给 3 秒自愈，仍断着再回退。
                    scope.launch {
                        delay(3_000)
                        if (closed || !adopted) return@launch
                        val current = pc?.iceConnectionState()
                        if (current == PeerConnection.IceConnectionState.DISCONNECTED ||
                            current == PeerConnection.IceConnectionState.FAILED
                        ) {
                            lost("直连已断开")
                        }
                    }
                }

                else -> Unit
            }
        }

        override fun onIceConnectionReceivingChange(receiving: Boolean) = Unit
        override fun onIceGatheringChange(state: PeerConnection.IceGatheringState?) = Unit

        override fun onIceCandidate(candidate: IceCandidate?) {
            val ice = candidate ?: return
            if (ice.sdp.isEmpty()) return
            val value = JSONObject()
                .put("candidate", ice.sdp)
                .put("sdpMid", ice.sdpMid)
                .put("sdpMLineIndex", ice.sdpMLineIndex)
            sendSignal("ice", JSONObject().put("ice", value))
        }

        override fun onIceCandidatesRemoved(candidates: Array<out IceCandidate>?) = Unit
        override fun onAddStream(stream: MediaStream?) = Unit
        override fun onRemoveStream(stream: MediaStream?) = Unit
        override fun onDataChannel(channel: DataChannel?) = Unit
        override fun onRenegotiationNeeded() = Unit

        override fun onTrack(transceiver: RtpTransceiver?) {
            val track = transceiver?.receiver?.track() ?: return
            when (track) {
                is VideoTrack -> {
                    // 不额外挂统计用的 sink：帧的引用计数由 WebRTC 自己管，
                    // sink 只借用、不能 release（否则渲染线程会二次释放而崩溃）。
                    // 画面尺寸从 getStats() 的 frameWidth/frameHeight 取。
                    remoteVideo = track
                    onVideoTrack(track)
                }

                is AudioTrack -> {
                    remoteAudio = track
                    track.setEnabled(audioWanted)
                }

                else -> Unit
            }
        }
    }

    // ---------------------------------------------------------------- DataChannel

    private val dcObserver = object : DataChannel.Observer {
        override fun onBufferedAmountChange(previousAmount: Long) = Unit

        override fun onStateChange() {
            val channel = dc ?: return
            when (channel.state()) {
                DataChannel.State.OPEN -> {
                    if (!adopted && !closed) {
                        openRequest = SESSION_SEQ.incrementAndGet()
                        channel.send(
                            DataChannel.Buffer(
                                ByteBuffer.wrap(
                                    JSONObject()
                                        .put("t", "open")
                                        .put("request", openRequest)
                                        .put("viewer", viewerName)
                                        .toString()
                                        .toByteArray(),
                                ),
                                false,
                            ),
                        )
                    }
                }

                DataChannel.State.CLOSED -> {
                    if (adopted) lost("直连通道已关闭") else failConnect("直连通道已关闭")
                }

                else -> Unit
            }
        }

        override fun onMessage(buffer: DataChannel.Buffer) {
            val bytes = ByteArray(buffer.data.remaining())
            buffer.data.get(bytes)
            if (buffer.binary) {
                if (adopted) viewer.handleP2pBinary(bytes)
                return
            }
            val text = String(bytes, Charsets.UTF_8)
            val message = runCatching { JSONObject(text) }.getOrNull() ?: return
            when (message.optString("t")) {
                "open.ok" -> {
                    if (message.optLong("request", 0) == openRequest.toLong()) {
                        val session = message.optInt("session", 0)
                        pendingOpen?.complete(session)
                        pendingOpen = null
                    }
                }

                "open.err" -> {
                    if (message.optLong("request", 0) == openRequest.toLong()) {
                        pendingOpen?.complete(-1)
                        pendingOpen = null
                    }
                }

                else -> if (adopted) viewer.handleP2pText(text)
            }
        }
    }

    // ---------------------------------------------------------------- 统计

    private fun startStats() {
        scope.launch {
            while (!closed) {
                delay(1_000)
                val peer = pc ?: break
                val report = withTimeoutOrNull(2_000) { statsOf(peer) } ?: continue
                if (closed) break
                var bytes = 0L
                var fps = 0
                var rtt = lastRtt
                var width = lastWidth
                var height = lastHeight
                for (stat in report.statsMap.values) {
                    val members = stat.members
                    when (stat.type) {
                        "inbound-rtp" -> {
                            val kind = members["kind"]?.toString() ?: members["mediaType"]?.toString()
                            if (kind == "video") {
                                bytes = members.number("bytesReceived")?.toLong() ?: bytes
                                fps = members.number("framesPerSecond")?.toInt() ?: fps
                                width = members.number("frameWidth")?.toInt() ?: width
                                height = members.number("frameHeight")?.toInt() ?: height
                            }
                        }

                        "candidate-pair" -> {
                            val selected = members["selected"]?.toString() == "true" ||
                                members["state"]?.toString() == "succeeded"
                            val value = members.number("currentRoundTripTime")
                            if (selected && value != null && value >= 0) {
                                rtt = (value * 1000).toLong()
                            }
                        }
                    }
                }
                val now = System.nanoTime()
                if (statsAt == 0L) {
                    statsAt = now
                    statsBytes = bytes
                }
                val seconds = ((now - statsAt).coerceAtLeast(1) / 1_000_000_000.0)
                val kbps = if (statsAt == 0L) 0 else {
                    (((bytes - statsBytes).coerceAtLeast(0) * 8) / seconds / 1000).toInt()
                }
                statsBytes = bytes
                statsAt = now
                lastRtt = rtt
                lastWidth = width
                lastHeight = height
                stats = P2pStats(fps, kbps, rtt, width, height)
            }
        }
    }

    private suspend fun statsOf(peer: PeerConnection): org.webrtc.RTCStatsReport =
        suspendCancellableCoroutine { continuation ->
            peer.getStats { report ->
                if (continuation.isActive) continuation.resume(report)
            }
        }

    // ---------------------------------------------------------------- 收尾

    private fun failConnect(reason: String?): Int? {
        if (adopted) return null
        pendingOpen?.let { if (!it.isCompleted) it.complete(-1) }
        pendingOpen = null
        sendSignal("bye", JSONObject())
        closed = true
        teardown()
        return null
    }

    private fun lost(reason: String) {
        if (closed) return
        closed = true
        val wasAdopted = adopted
        adopted = false
        teardown()
        if (wasAdopted) viewer.channelLost(reason)
    }

    private fun teardown() {
        pendingOpen?.let { if (!it.isCompleted) it.complete(-1) }
        pendingOpen = null
        runCatching { dc?.unregisterObserver() }
        runCatching { dc?.close() }
        dc = null
        remoteVideo = null
        remoteAudio = null
        runCatching { pc?.close() }
        runCatching { pc?.dispose() }
        pc = null
        synchronized(lock) { iceQueue.clear() }
        remoteSet = false
        onVideoTrack(null)
        stats = P2pStats()
        scope.cancel()
    }

    private companion object {
        /** 与网页端一致：3 秒连不上就回退中继；模拟器上多留一点余量。 */
        const val CONNECT_TIMEOUT_MS = 4_000L
        val SIGNAL_SEQ = AtomicInteger(0)
        val SESSION_SEQ = AtomicInteger(0)
    }
}

private fun Map<String, Any?>.number(key: String): Double? = when (val value = this[key]) {
    is Number -> value.toDouble()
    is String -> value.toDoubleOrNull()
    else -> null
}
