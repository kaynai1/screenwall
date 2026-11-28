package com.screenwall.android.net

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeout
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.json.JSONArray
import org.json.JSONObject
import java.net.URLEncoder
import java.util.concurrent.atomic.AtomicLong

/** `open.err` 带出的错误码与说明。 */
class OpenError(val code: Int, val msg: String) : Exception("$code: $msg")

/**
 * 主控端 WebSocket（`/ws/view?token=…`）。
 *
 * 服务端先发纯文本的 `ready` / `devices` / `open.ok` / `open.err` / `closed`；
 * 会话内的应用消息与媒体帧都是 8 字节会话帧包着的二进制。
 */
class ViewerClient(private val http: OkHttpClient) {

    interface Listener {
        fun onSocketReady() {}
        fun onDevices(devices: List<JSONObject>) {}
        fun onOpenOk(session: Int) {}
        fun onControlClosed(code: Int, msg: String) {}
        fun onSessionMessage(message: JSONObject) {}
        fun onFrame(frame: MediaFrame) {}
        fun onSocketClosed(reason: String) {}
    }

    @Volatile
    var listener: Listener? = null

    private var socket: WebSocket? = null

    @Volatile
    var session: Int = 0
        private set

    private var ready: CompletableDeferred<Unit>? = null
    private var pendingOpen: CompletableDeferred<Int>? = null
    private var pendingRequest: Long = -1
    private val requestSeq = AtomicLong(0)
    private var closedByUs = false
    private var closeNotified = false

    val isOpen: Boolean get() = socket != null && ready?.isCompleted == true

    /** 连上并等到 `ready`；失败抛异常。 */
    suspend fun connect(serverUrl: String, token: String) {
        disconnect()
        closedByUs = false
        closeNotified = false
        session = 0
        val base = when {
            serverUrl.startsWith("https://", ignoreCase = true) -> "wss://" + serverUrl.substring(8)
            serverUrl.startsWith("http://", ignoreCase = true) -> "ws://" + serverUrl.substring(7)
            else -> "ws://$serverUrl"
        }.trimEnd('/')
        val encoded = URLEncoder.encode(token, "UTF-8")
        val url = "$base/ws/view?token=$encoded"
        val deferred = CompletableDeferred<Unit>()
        ready = deferred
        val request = Request.Builder().url(url).build()
        val webSocket = http.newWebSocket(
            request,
            object : WebSocketListener() {
                override fun onMessage(webSocket: WebSocket, text: String) {
                    handleText(text, deferred, webSocket)
                }

                override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                    handleBinary(bytes.toByteArray())
                }

                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(1000, null)
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    handleClosed(webSocket, "连接已断开（$code）")
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    if (!deferred.isCompleted) deferred.completeExceptionally(t)
                    handleClosed(webSocket, t.message ?: "网络连接中断")
                }
            },
        )
        socket = webSocket
        try {
            withTimeout(15_000) { deferred.await() }
        } catch (error: Throwable) {
            socket = null
            webSocket.cancel()
            throw error
        }
    }

    /** 请求打开设备会话，返回会话号。 */
    suspend fun open(deviceId: String, viewerName: String, mode: String = "control"): Int {
        val webSocket = socket ?: throw IllegalStateException("连接尚未就绪")
        val request = requestSeq.incrementAndGet()
        val deferred = CompletableDeferred<Int>()
        pendingOpen = deferred
        pendingRequest = request
        val message = JSONObject()
            .put("t", "open")
            .put("request", request)
            .put("deviceId", deviceId)
            .put("viewer", viewerName)
            .put("mode", mode)
            .toString()
        if (!webSocket.send(message)) {
            pendingOpen = null
            throw IllegalStateException("连接已断开")
        }
        return try {
            // 被控端有 30 秒授权窗口，这里多留一点余量。
            withTimeout(35_000) { deferred.await() }
        } finally {
            if (pendingOpen === deferred) pendingOpen = null
        }
    }

    /** 发一条隧道内的应用消息。会话还没建立时静默丢弃。 */
    fun sendSession(message: JSONObject): Boolean {
        val current = session
        val webSocket = socket ?: return false
        if (current == 0) return false
        val bytes = Protocol.tunnelJson(current, message.toString())
        return webSocket.send(bytes.toByteString())
    }

    fun sendText(message: JSONObject): Boolean = socket?.send(message.toString()) == true

    /** 只关会话，保留到服务器的连接（从控制页返回设备列表时用）。 */
    fun closeSession() {
        val current = session
        if (current != 0) {
            sendText(JSONObject().put("t", "close").put("session", current))
        }
        session = 0
    }

    fun disconnect() {
        closedByUs = true
        closeSession()
        socket?.close(1000, "bye")
        socket = null
        ready = null
        pendingOpen?.let { if (!it.isCompleted) it.completeExceptionally(IllegalStateException("连接已断开")) }
        pendingOpen = null
    }

    private fun handleText(text: String, deferred: CompletableDeferred<Unit>, webSocket: WebSocket) {
        val message = runCatching { JSONObject(text) }.getOrNull() ?: return
        when (message.optString("t")) {
            "ready" -> {
                if (!deferred.isCompleted) deferred.complete(Unit)
                listener?.onSocketReady()
            }
            "devices" -> {
                val array = message.optJSONArray("devices") ?: JSONArray()
                val list = (0 until array.length()).mapNotNull { array.optJSONObject(it) }
                listener?.onDevices(list)
            }
            "open.ok" -> {
                val request = message.optLong("request", -1)
                if (request == pendingRequest && pendingOpen !== null) {
                    val id = message.optInt("session", 0)
                    session = id
                    val pending = pendingOpen
                    pendingOpen = null
                    pending?.complete(id)
                    listener?.onOpenOk(id)
                }
            }
            "open.err" -> {
                val request = message.optLong("request", -1)
                val code = message.optInt("code", 4204)
                val msg = message.optString("msg", "设备拒绝了会话")
                if (request == pendingRequest && pendingOpen !== null) {
                    val pending = pendingOpen
                    pendingOpen = null
                    pending?.completeExceptionally(OpenError(code, msg))
                }
            }
            "closed" -> {
                session = 0
                listener?.onControlClosed(message.optInt("code", 4201), message.optString("msg", "会话已关闭"))
            }
            "pong" -> listener?.onSessionMessage(message)
            else -> listener?.onSessionMessage(message)
        }
    }

    private fun handleBinary(bytes: ByteArray) {
        val tunnel = Protocol.decodeTunnel(bytes) ?: return
        when (tunnel.kind) {
            Protocol.TUNNEL_JSON -> {
                val message = runCatching { JSONObject(String(tunnel.payload, Charsets.UTF_8)) }.getOrNull()
                if (message != null) listener?.onSessionMessage(message)
            }
            Protocol.TUNNEL_BINARY -> {
                val frame = Protocol.decodeMediaFrame(tunnel.payload) ?: return
                listener?.onFrame(frame)
            }
        }
    }

    private fun handleClosed(webSocket: WebSocket, reason: String) {
        if (webSocket !== socket || closedByUs || closeNotified) return
        closeNotified = true
        socket = null
        session = 0
        ready?.let { if (!it.isCompleted) it.completeExceptionally(IllegalStateException(reason)) }
        ready = null
        pendingOpen?.let { if (!it.isCompleted) it.completeExceptionally(IllegalStateException(reason)) }
        pendingOpen = null
        listener?.onSocketClosed(reason)
    }
}
