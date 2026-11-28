package com.screenwall.android

import android.content.Context
import android.graphics.Bitmap
import com.screenwall.android.data.Account
import com.screenwall.android.session.InputMode
import com.screenwall.android.session.StreamOptions
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/** 最近连接过的一台设备。 */
data class RecentDevice(
    val id: String,
    val name: String,
    val os: String,
    val lastAt: Long,
)

/** 画面偏好预设：和网页端/桌面端的「帧率优先 / 画质优先 / 平衡」同一套档位。 */
enum class PicturePreset(val label: String, val maxWidth: Int, val fps: Int, val quality: Int) {
    Fps("帧率优先", 1280, 60, 45),
    /** 0 = 不设上限，被控端只在桌面变化时出帧，实际跟着显示器刷新率走。 */
    Auto("自动帧率", 1280, 0, 65),
    Quality("画质优先", 1920, 30, 80),
    Balanced("平衡", 1280, 30, 65),
}

/** 本机持久化：服务器地址、上次登录的账号、画质选项、最近连接、预览图。 */
class Prefs(context: Context) {
    private val appContext = context.applicationContext
    private val sp = appContext.getSharedPreferences("screenwall", Context.MODE_PRIVATE)

    var serverUrl: String
        get() = sp.getString(KEY_SERVER, DEFAULT_SERVER) ?: DEFAULT_SERVER
        set(value) {
            sp.edit().putString(KEY_SERVER, value).apply()
        }

    var username: String
        get() = sp.getString(KEY_USERNAME, "") ?: ""
        set(value) {
            sp.edit().putString(KEY_USERNAME, value).apply()
        }

    /** 令牌只用于自动登录；退出登录时清空。 */
    private var token: String
        get() = sp.getString(KEY_TOKEN, "") ?: ""
        set(value) {
            sp.edit().putString(KEY_TOKEN, value).apply()
        }

    private var deviceId: String
        get() = sp.getString(KEY_DEVICE_ID, "") ?: ""
        set(value) {
            sp.edit().putString(KEY_DEVICE_ID, value).apply()
        }

    fun loadAccount(): Account? {
        val token = token
        val username = username
        if (token.isBlank() || username.isBlank()) return null
        return Account(serverUrl, token, username, deviceId)
    }

    fun saveAccount(account: Account) {
        // 识别码会话的令牌 15 分钟就失效，不写进自动登录。
        if (account.quick) return
        serverUrl = account.serverUrl
        username = account.username
        token = account.token
        deviceId = account.deviceId
    }

    fun clearAccount() {
        token = ""
        deviceId = ""
    }

    fun streamOptions(): StreamOptions {
        val raw = sp.getString(KEY_OPTIONS, null) ?: return StreamOptions()
        return runCatching {
            val parts = raw.split(',')
            StreamOptions(
                maxWidth = parts.getOrNull(0)?.toIntOrNull() ?: 1280,
                fps = parts.getOrNull(1)?.toIntOrNull() ?: 30,
                quality = parts.getOrNull(2)?.toIntOrNull() ?: 65,
                preferH264 = parts.getOrNull(3) != "0",
            )
        }.getOrElse { StreamOptions() }
    }

    fun saveStreamOptions(options: StreamOptions) {
        sp.edit()
            .putString(
                KEY_OPTIONS,
                listOf(
                    options.maxWidth,
                    options.fps,
                    options.quality,
                    if (options.preferH264) 1 else 0,
                ).joinToString(","),
            )
            .apply()
    }

    /** 当前选项对应的预设；用户手动改过档位时返回 null。 */
    fun picturePreset(): PicturePreset? {
        val options = streamOptions()
        return PicturePreset.entries.firstOrNull {
            it.maxWidth == options.maxWidth && it.fps == options.fps && it.quality == options.quality
        }
    }

    /** 应用预设，保留用户对 H.264 的偏好。 */
    fun applyPreset(preset: PicturePreset) {
        val current = streamOptions()
        saveStreamOptions(
            current.copy(
                maxWidth = preset.maxWidth,
                fps = preset.fps,
                quality = preset.quality,
            ),
        )
    }

    var inputMode: InputMode
        get() = if (sp.getBoolean(KEY_TRACKPAD, false)) InputMode.Trackpad else InputMode.Absolute
        set(value) {
            sp.edit().putBoolean(KEY_TRACKPAD, value == InputMode.Trackpad).apply()
        }

    /** 连接建立后锁定横屏。 */
    var lockLandscape: Boolean
        get() = sp.getBoolean(KEY_LOCK_LANDSCAPE, false)
        set(value) {
            sp.edit().putBoolean(KEY_LOCK_LANDSCAPE, value).apply()
        }

    /** 退出会话时保存最后一帧，用作最近连接缩略图。 */
    var savePreview: Boolean
        get() = sp.getBoolean(KEY_SAVE_PREVIEW, false)
        set(value) {
            sp.edit().putBoolean(KEY_SAVE_PREVIEW, value).apply()
        }

    // ------------------------------------------------------------ 最近连接

    fun recents(): List<RecentDevice> {
        val raw = sp.getString(KEY_RECENTS, null) ?: return emptyList()
        return runCatching {
            val array = JSONArray(raw)
            (0 until array.length()).mapNotNull { index ->
                val item = array.optJSONObject(index) ?: return@mapNotNull null
                val id = item.optString("id")
                if (id.isBlank()) return@mapNotNull null
                RecentDevice(
                    id = id,
                    name = item.optString("name").ifBlank { "未命名设备" },
                    os = item.optString("os"),
                    lastAt = item.optLong("lastAt"),
                )
            }
        }.getOrElse { emptyList() }
    }

    fun addRecent(id: String, name: String, os: String) {
        if (id.isBlank()) return
        val next = buildList {
            add(RecentDevice(id, name, os, System.currentTimeMillis()))
            addAll(recents().filter { it.id != id })
        }.take(MAX_RECENTS)
        val array = JSONArray()
        next.forEach { item ->
            array.put(
                JSONObject()
                    .put("id", item.id)
                    .put("name", item.name)
                    .put("os", item.os)
                    .put("lastAt", item.lastAt),
            )
        }
        sp.edit().putString(KEY_RECENTS, array.toString()).apply()
    }

    fun clearRecents() {
        sp.edit().remove(KEY_RECENTS).apply()
        clearPreviews()
    }

    // ------------------------------------------------------------ 预览图

    private fun previewDir(): File = File(appContext.filesDir, "previews")

    fun previewFile(id: String): File? {
        if (id.isBlank()) return null
        val safe = id.replace(Regex("[^A-Za-z0-9._-]"), "_")
        return File(previewDir(), "$safe.png")
    }

    fun savePreview(id: String, bitmap: Bitmap) {
        val file = previewFile(id) ?: return
        runCatching {
            file.parentFile?.mkdirs()
            file.outputStream().use { out ->
                bitmap.compress(Bitmap.CompressFormat.JPEG, 82, out)
            }
        }
    }

    fun clearPreviews() {
        runCatching { previewDir().listFiles()?.forEach { it.delete() } }
    }

    fun hasPreviews(): Boolean = previewDir().listFiles()?.isNotEmpty() == true

    companion object {
        const val DEFAULT_SERVER = "http://127.0.0.1:8788"
        const val MAX_RECENTS = 8
        private const val KEY_SERVER = "serverUrl"
        private const val KEY_USERNAME = "username"
        private const val KEY_TOKEN = "token"
        private const val KEY_DEVICE_ID = "deviceId"
        private const val KEY_OPTIONS = "streamOptions"
        private const val KEY_TRACKPAD = "trackpad"
        private const val KEY_RECENTS = "recents"
        private const val KEY_LOCK_LANDSCAPE = "lockLandscape"
        private const val KEY_SAVE_PREVIEW = "savePreview"
    }
}
