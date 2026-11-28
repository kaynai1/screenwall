package com.screenwall.android.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.withContext
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/** 一次可以在应用里提示的安卓端更新。 */
data class AppUpdate(
    val version: String,
    val notes: String,
    /** 安装包直链；为空时退到发版页。 */
    val apkUrl: String,
    val pageUrl: String,
) {
    /** 优先下载安装包，没有直链就打开发版页。 */
    val downloadUrl: String get() = apkUrl.ifBlank { pageUrl }
}

/**
 * 更新检查与服务端版本查询。
 *
 * 安卓端有两个更新源，谁给出的版本更新就用谁：
 * 1. 服务器 `GET /api/v1/version`：只在 `url` 以 `.apk` 结尾时当作安卓更新。
 *    这个接口在桌面端返回的是 `.exe` 安装包，不能拿它的版本号和安卓版比。
 * 2. GitHub 最新 Release：取带 `.apk` 附件的版本。
 */
object Updates {
    private const val REPO = "kaynai1/screenwall"
    private val versionRe = Regex("""(\d+)\.(\d+)\.(\d+)""")

    /** `GET /healthz` 里的服务端自身版本；连不上返回 null。 */
    suspend fun serverVersion(http: OkHttpClient, serverUrl: String): String? =
        withContext(Dispatchers.IO) {
            val url = serverUrl.trimEnd('/') + "/healthz"
            runCatching {
                http.newCall(
                    Request.Builder()
                        .url(url)
                        .header("User-Agent", "ScreenWall-Android/${AppInfo.VERSION}")
                        .build(),
                ).execute().use { response ->
                    if (!response.isSuccessful) return@use null
                    JSONObject(response.body?.string().orEmpty())
                        .optString("version")
                        .ifBlank { null }
                }
            }.getOrNull()
        }

    /**
     * 检查更新：返回比 [current] 新的版本，没有更新返回 null。
     * 两个源都没有回应时抛出异常，设置页可据此说明失败原因。
     */
    suspend fun check(
        http: OkHttpClient,
        serverUrl: String,
        current: String = AppInfo.VERSION,
    ): AppUpdate? = withContext(Dispatchers.IO) {
        coroutineScope {
            // 两个源并发查，各有自己的超时：GitHub 不通时不让首页的提示一直等。
            val server = async { runCatching { fromServer(http, serverUrl) } }
            val github = async { runCatching { fromGitHub(http) } }
            val results = listOf(server.await(), github.await())

            var best: AppUpdate? = null
            var answered = false
            var lastError: Throwable? = null
            for (result in results) {
                result
                    .onSuccess {
                        answered = true
                        if (it != null) best = newer(best, it)
                    }
                    .onFailure { lastError = it }
            }

            val candidate = best
            when {
                candidate == null && !answered ->
                    throw lastError ?: IllegalStateException("检查更新失败")
                candidate == null -> null
                isNewer(candidate.version, current) -> candidate
                else -> null
            }
        }
    }

    /** OkHttp 的全局 client 没设置单次超时，更新检查单独限一下。 */
    private fun execute(http: OkHttpClient, request: Request, seconds: Long): Response {
        val call = http.newCall(request)
        call.timeout().timeout(seconds, TimeUnit.SECONDS)
        return call.execute()
    }

    private fun fromServer(http: OkHttpClient, serverUrl: String): AppUpdate? {
        val url = serverUrl.trimEnd('/') + "/api/v1/version"
        execute(
            http,
            Request.Builder()
                .url(url)
                .header("User-Agent", "ScreenWall-Android/${AppInfo.VERSION}")
                .build(),
            4,
        ).use { response ->
            // 服务器没配置更新（桌面端常见）就不算安卓更新源。
            if (response.code == 404) return null
            if (!response.isSuccessful) throw IllegalStateException("服务器返回 ${response.code}")
            val data = JSONObject(response.body?.string().orEmpty())
            val raw = data.optString("url")
            if (!raw.endsWith(".apk", ignoreCase = true)) return null
            val absolute = if (raw.startsWith("http://") || raw.startsWith("https://")) {
                raw
            } else {
                serverUrl.trimEnd('/') + if (raw.startsWith("/")) raw else "/$raw"
            }
            return AppUpdate(
                version = data.optString("version"),
                notes = data.optString("notes"),
                apkUrl = absolute,
                pageUrl = absolute,
            )
        }
    }

    private fun fromGitHub(http: OkHttpClient): AppUpdate? {
        val request = Request.Builder()
            .url("https://api.github.com/repos/$REPO/releases?per_page=30")
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "ScreenWall-Android/${AppInfo.VERSION}")
            .build()
        execute(http, request, 6).use { response ->
            if (!response.isSuccessful) throw IllegalStateException("GitHub 返回 ${response.code}")
            val releases = JSONArray(response.body?.string().orEmpty())
            var best: AppUpdate? = null
            for (index in 0 until releases.length()) {
                val release = releases.optJSONObject(index) ?: continue
                if (release.optBoolean("draft") || release.optBoolean("prerelease")) continue
                val assets = release.optJSONArray("assets") ?: JSONArray()
                var apk: String? = null
                for (assetIndex in 0 until assets.length()) {
                    val asset = assets.optJSONObject(assetIndex) ?: continue
                    if (asset.optString("name").endsWith(".apk", ignoreCase = true)) {
                        apk = asset.optString("browser_download_url").ifBlank { null }
                        break
                    }
                }
                if (apk == null) continue
                val version = release.optString("tag_name")
                if (version.isBlank()) continue
                val candidate = AppUpdate(
                    version = version,
                    notes = release.optString("body"),
                    apkUrl = apk,
                    pageUrl = release.optString("html_url"),
                )
                best = newer(best, candidate)
            }
            return best
        }
    }

    private fun newer(left: AppUpdate?, right: AppUpdate): AppUpdate =
        if (left == null || compare(right.version, left.version) > 0) right else left

    private fun isNewer(candidate: String, current: String): Boolean =
        compare(candidate, current) > 0

    private fun compare(left: String, right: String): Int {
        val a = parts(left) ?: return 0
        val b = parts(right) ?: return 0
        for (index in 0 until maxOf(a.size, b.size)) {
            val x = a.getOrElse(index) { 0 }
            val y = b.getOrElse(index) { 0 }
            if (x != y) return x.compareTo(y)
        }
        return 0
    }

    private fun parts(version: String): List<Int>? =
        versionRe.find(version)?.groupValues?.drop(1)?.mapNotNull { it.toIntOrNull() }

    /** 去掉 tag 里的 `v` 前缀，显示成 `0.1.1`。 */
    fun pretty(version: String): String = version.trim().removePrefix("v").removePrefix("V")
}
