package com.screenwall.android

import android.content.Context
import com.screenwall.android.data.Account
import com.screenwall.android.session.InputMode
import com.screenwall.android.session.StreamOptions

/** 本机持久化：服务器地址、上次登录的账号、画质选项。 */
class Prefs(context: Context) {
    private val sp = context.getSharedPreferences("screenwall", Context.MODE_PRIVATE)

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

    var inputMode: InputMode
        get() = if (sp.getBoolean(KEY_TRACKPAD, false)) InputMode.Trackpad else InputMode.Absolute
        set(value) {
            sp.edit().putBoolean(KEY_TRACKPAD, value == InputMode.Trackpad).apply()
        }

    companion object {
        const val DEFAULT_SERVER = "http://60.204.193.130:8787"
        private const val KEY_SERVER = "serverUrl"
        private const val KEY_USERNAME = "username"
        private const val KEY_TOKEN = "token"
        private const val KEY_DEVICE_ID = "deviceId"
        private const val KEY_OPTIONS = "streamOptions"
        private const val KEY_TRACKPAD = "trackpad"
    }
}
