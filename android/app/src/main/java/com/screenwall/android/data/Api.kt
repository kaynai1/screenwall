package com.screenwall.android.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import org.json.JSONArray
import org.json.JSONObject

/** 登录后的账号信息。令牌只放在内存与 SharedPreferences 里，不写日志。 */
data class Account(
    val serverUrl: String,
    val token: String,
    val username: String,
    val deviceId: String,
    /** 识别码换来的受限会话：只能连一台设备，令牌 15 分钟失效。 */
    val quick: Boolean = false,
)

/** 客户端对外声明的名称与版本（hello / 登录 / 关于页共用）。 */
object AppInfo {
    const val NAME = "ScreenWall 安卓端"
    const val CLIENT_NAME = "安卓端"
    const val VERSION = "0.1.1"
}

/** `GET /api/v1/devices` 与 WS `devices` 消息共用的设备视图。 */
data class Device(
    val id: String,
    val name: String,
    val os: String,
    val version: String,
    val online: Boolean,
    val lastSeen: Long,
    val controllable: Boolean,
    /** 被控端支持网页/手机端 WebRTC 直连（服务器把 `webrtc=1` 的连接标在这里）。 */
    val webrtc: Boolean = false,
) {
    companion object {
        fun fromJson(value: JSONObject): Device = Device(
            id = value.optString("id"),
            name = value.optString("name").ifBlank { "未命名设备" },
            os = value.optString("os"),
            version = value.optString("version"),
            online = value.optBoolean("online"),
            lastSeen = value.optLong("lastSeen"),
            controllable = value.optBoolean("controllable", true),
            webrtc = value.optBoolean("webrtc", false),
        )
    }
}

/**
 * 查看端（安卓 / 网页 / iOS 客户端）也会以「设备」身份出现在账号里，
 * 但它们不是被控端，放进设备列表只会造成干扰，列表里统一隐藏。
 */
val Device.isViewerClient: Boolean
    get() {
        val value = os.trim().lowercase()
        return value.startsWith("android") || value.startsWith("web") ||
            value.startsWith("ios") || value == "iphone" || value == "ipad"
    }

class ApiException(
    val code: String,
    val msg: String,
    val status: Int,
) : Exception(msg)

/** 账号接口：登录、设备列表、退出。WebSocket 在 [com.screenwall.android.net.ViewerClient]。 */
class Api(private val http: OkHttpClient) {
    private val jsonType = "application/json; charset=utf-8".toMediaType()

    suspend fun login(serverUrl: String, username: String, password: String): Account =
        withContext(Dispatchers.IO) {
            val body = JSONObject()
                .put("username", username)
                .put("password", password)
                .put(
                    "device",
                    JSONObject()
                        .put("name", AppInfo.CLIENT_NAME)
                        .put("os", "Android")
                        .put("version", android.os.Build.VERSION.RELEASE ?: "android"),
                )
            val data = postJson(serverUrl, "/api/v1/login", body)
            val user = data.optJSONObject("user")
            val device = data.optJSONObject("device")
            Account(
                serverUrl = serverUrl,
                token = data.optString("token"),
                username = user?.optString("username").orEmpty().ifBlank { username },
                deviceId = device?.optString("id").orEmpty(),
            )
        }

    /**
     * 识别码 + 密码，换一个只能打开这台设备的受限令牌，不需要账号。
     * 对应服务端 `POST /api/v1/quick/connect`，和网页端的「识别码」入口同一条路。
     */
    suspend fun quickConnect(serverUrl: String, code: String, password: String): Account =
        withContext(Dispatchers.IO) {
            val body = JSONObject()
                .put("code", code.trim())
                .put("password", password)
                .put("name", AppInfo.CLIENT_NAME)
            val data = postJson(serverUrl, "/api/v1/quick/connect", body)
            val device = data.optJSONObject("device")
            Account(
                serverUrl = serverUrl,
                token = data.optString("token"),
                username = device?.optString("name").orEmpty().ifBlank { "远程设备" },
                deviceId = device?.optString("id").orEmpty(),
                quick = true,
            )
        }

    suspend fun devices(account: Account): List<Device> = withContext(Dispatchers.IO) {
        val request = Request.Builder()
            .url("${account.serverUrl}/api/v1/devices")
            .header("authorization", "Bearer ${account.token}")
            .get()
            .build()
        http.newCall(request).execute().use { response ->
            val text = response.body?.string().orEmpty()
            if (!response.isSuccessful) throw parseError(text, response.code)
            parseDevices(JSONObject(text).optJSONArray("devices"))
        }
    }

    suspend fun logout(account: Account) {
        withContext(Dispatchers.IO) {
            runCatching {
                val request = Request.Builder()
                    .url("${account.serverUrl}/api/v1/logout")
                    .header("authorization", "Bearer ${account.token}")
                    .post(ByteArray(0).toRequestBody(null))
                    .build()
                http.newCall(request).execute().close()
            }
        }
    }

    private fun postJson(serverUrl: String, path: String, body: JSONObject): JSONObject {
        val request = Request.Builder()
            .url("$serverUrl$path")
            .post(body.toString().toRequestBody(jsonType))
            .build()
        http.newCall(request).execute().use { response ->
            val text = response.body?.string().orEmpty()
            if (!response.isSuccessful) throw parseError(text, response.code)
            return runCatching { JSONObject(text) }.getOrElse {
                throw ApiException("bad_response", "服务器返回了无法解析的内容", response.code)
            }
        }
    }

    private fun parseError(text: String, status: Int): ApiException {
        val data = runCatching { JSONObject(text) }.getOrNull()
        val code = data?.optString("error").orEmpty().ifBlank { "http_error" }
        val msg = data?.optString("msg").orEmpty().ifBlank { "HTTP $status" }
        return ApiException(code, msg, status)
    }

    companion object {
        fun parseDevices(array: JSONArray?): List<Device> {
            if (array == null) return emptyList()
            return (0 until array.length()).mapNotNull { index ->
                array.optJSONObject(index)?.let(Device::fromJson)
            }
        }

        /** 补协议、去尾斜杠；非法地址返回 null。 */
        fun normalizeServerUrl(input: String): String? {
            val value = input.trim()
            if (value.isEmpty()) return null
            val withScheme = if (Regex("^https?://", RegexOption.IGNORE_CASE).containsMatchIn(value)) {
                value
            } else {
                "http://$value"
            }
            val normalized = withScheme.trimEnd('/')
            return if (normalized.toHttpUrlOrNull() == null) null else normalized
        }
    }
}
