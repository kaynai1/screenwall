package com.screenwall.android.ui

import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ArrowBack
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.PicturePreset
import com.screenwall.android.Prefs
import com.screenwall.android.ThemeMode
import com.screenwall.android.data.Api
import com.screenwall.android.data.AppInfo
import com.screenwall.android.data.AppUpdate
import com.screenwall.android.data.Updates
import com.screenwall.android.session.AppSession
import com.screenwall.android.session.InputMode
import kotlinx.coroutines.launch

/**
 * 全局设置页（对应 CrossDesk 的设置页）：鼠标模式、画面偏好、横屏锁定、
 * 服务器、本机 ID、隐私与预览图、本机配对信息、关于。
 *
 * 会话内的画质微调仍在控制页的「会话设置」里，这里只放跨会话生效的选项。
 */
@Composable
fun SettingsScreen(
    session: AppSession,
    prefs: Prefs,
    themeMode: ThemeMode,
    onThemeMode: (ThemeMode) -> Unit,
    onBack: () -> Unit,
    onLogout: () -> Unit,
    onServerChanged: (String) -> Unit,
) {
    SystemBars(lightIcons = !LocalDarkTheme.current)
    val context = LocalContext.current
    val clipboard = LocalClipboardManager.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }

    var inputMode by remember { mutableStateOf(prefs.inputMode) }
    var preset by remember { mutableStateOf(prefs.picturePreset()) }
    var lockLandscape by remember { mutableStateOf(prefs.lockLandscape) }
    var savePreview by remember { mutableStateOf(prefs.savePreview) }
    var hasPreviews by remember { mutableStateOf(prefs.hasPreviews()) }

    var serverOpen by remember { mutableStateOf(false) }
    var serverInput by remember { mutableStateOf(prefs.serverUrl) }
    var serverError by remember { mutableStateOf("") }
    var pairingOpen by remember { mutableStateOf(false) }
    var aboutOpen by remember { mutableStateOf(false) }
    var serverVersion by remember { mutableStateOf<String?>(null) }
    var serverVersionLoading by remember { mutableStateOf(false) }
    var checkingUpdate by remember { mutableStateOf(false) }
    var updateMessage by remember { mutableStateOf("") }
    var updateFound by remember { mutableStateOf<AppUpdate?>(null) }

    val account = session.account
    val localId = if (account.quick) "" else account.deviceId

    BackHandler { onBack() }

    fun copy(text: String, tip: String) {
        clipboard.setText(AnnotatedString(text))
        scope.launch { snackbar.showSnackbar(tip) }
    }

    fun openAppSettings() {
        runCatching {
            context.startActivity(
                Intent(
                    Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.parse("package:${context.packageName}"),
                ),
            )
        }
    }

    fun openUrl(url: String) {
        runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url))) }
    }

    fun checkUpdates() {
        if (checkingUpdate) return
        checkingUpdate = true
        updateMessage = "正在检查…"
        updateFound = null
        scope.launch {
            try {
                val info = Updates.check(session.http, session.account.serverUrl)
                updateFound = info
                updateMessage = if (info == null) {
                    "已是最新版本"
                } else {
                    "发现新版本 ${Updates.pretty(info.version)}"
                }
            } catch (throwable: Throwable) {
                updateMessage = "检查失败：${throwable.message ?: "网络不可用"}"
            } finally {
                checkingUpdate = false
            }
        }
    }

    // 打开「关于」时顺手查一下服务端自己的版本（/healthz），失败就显示暂不可用。
    LaunchedEffect(aboutOpen) {
        if (!aboutOpen) return@LaunchedEffect
        serverVersionLoading = true
        serverVersion = Updates.serverVersion(session.http, session.account.serverUrl)
        serverVersionLoading = false
    }

    ScreenBackground {
        Box(Modifier.fillMaxSize()) {
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .statusBarsPadding(),
            ) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 8.dp, vertical = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    IconButton(onClick = onBack, modifier = Modifier.size(44.dp)) {
                        Icon(
                            imageVector = Icons.Filled.ArrowBack,
                            contentDescription = "返回",
                            tint = MaterialTheme.colorScheme.onSurface,
                        )
                    }
                    Box(Modifier.weight(1f), contentAlignment = Alignment.Center) {
                        Text(
                            text = "设置",
                            style = MaterialTheme.typography.titleMedium,
                            fontSize = 18.sp,
                        )
                    }
                    Spacer(Modifier.size(44.dp))
                }

                Column(
                    modifier = Modifier
                        .fillMaxSize()
                        .verticalScroll(rememberScrollState())
                        .navigationBarsPadding()
                        .padding(horizontal = 16.dp),
                ) {
                    SettingsSectionTitle("外观")
                    GroupCard {
                        Box(Modifier.padding(12.dp)) {
                            SegmentedControl(
                                options = ThemeMode.entries.map { it.name to it.label },
                                selected = themeMode.name,
                                onSelect = { name ->
                                    onThemeMode(ThemeMode.entries.first { it.name == name })
                                },
                            )
                        }
                    }
                    Caption("跟随系统会随手机的深色模式自动切换；浅色 / 深色只影响本机界面。")

                    SettingsSectionTitle("鼠标控制")
                    GroupCard {
                        Box(Modifier.padding(12.dp)) {
                            SegmentedControl(
                                options = listOf(
                                    InputMode.Trackpad to "相对位置",
                                    InputMode.Absolute to "绝对位置",
                                ),
                                selected = inputMode,
                                onSelect = {
                                    inputMode = it
                                    prefs.inputMode = it
                                },
                            )
                        }
                    }
                    Caption("相对位置：手指滑动移动光标，适合精细操作；绝对位置：点哪里远端光标就到哪里。")

                    SettingsSectionTitle("画面偏好")
                    GroupCard {
                        Box(Modifier.padding(12.dp)) {
                            SegmentedControl(
                                options = PicturePreset.entries.map { it.name to it.label },
                                selected = preset?.name ?: "",
                                onSelect = { name ->
                                    val next = PicturePreset.entries.first { it.name == name }
                                    prefs.applyPreset(next)
                                    preset = next
                                },
                            )
                        }
                    }
                    Caption("帧率优先适合看动画和视频，画质优先适合看文字和代码；会话内还可以单独微调。")

                    SettingsSectionTitle("屏幕方向")
                    GroupCard {
                        SettingsSwitchRow(
                            title = "连接建立后锁定横屏",
                            subtitle = "默认关闭。开启后，连接成功时进入并保持横屏；关闭时跟随设备方向。",
                            checked = lockLandscape,
                            onChange = {
                                lockLandscape = it
                                prefs.lockLandscape = it
                            },
                        )
                    }

                    SettingsSectionTitle("服务器")
                    GroupCard {
                        SettingsRow(
                            title = "服务器",
                            trailing = serverLabel(prefs.serverUrl),
                            showChevron = true,
                            onClick = {
                                serverInput = prefs.serverUrl
                                serverError = ""
                                serverOpen = true
                            },
                        )
                    }
                    Text(
                        text = if (localId.isBlank()) {
                            "连接码会话 · 本机 ID 不可用"
                        } else {
                            "本机 ID $localId"
                        },
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(top = 10.dp)
                            .clickable(enabled = localId.isNotBlank()) {
                                copy(localId, "已复制本机 ID")
                            },
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        fontSize = 13.sp,
                        textAlign = TextAlign.Center,
                    )

                    SettingsSectionTitle("隐私")
                    GroupCard {
                        SettingsRow(
                            title = "隐私与授权",
                            trailing = "已授权",
                            showChevron = true,
                            onClick = { openAppSettings() },
                        )
                        RowDivider()
                        SettingsSwitchRow(
                            title = "保存远程画面预览",
                            subtitle = "退出会话时保存最后一帧，用作最近连接缩略图（仅 MJPEG 画面）。",
                            checked = savePreview,
                            onChange = {
                                savePreview = it
                                prefs.savePreview = it
                            },
                        )
                        RowDivider()
                        SettingsRow(
                            title = "清除预览图",
                            danger = true,
                            enabled = hasPreviews,
                            onClick = {
                                prefs.clearPreviews()
                                hasPreviews = false
                                scope.launch { snackbar.showSnackbar("已清除预览图") }
                            },
                        )
                    }

                    Spacer(Modifier.height(12.dp))
                    GroupCard {
                        SettingsRow(
                            title = "本机配对信息",
                            showChevron = true,
                            onClick = { pairingOpen = true },
                        )
                    }

                    Spacer(Modifier.height(12.dp))
                    GroupCard {
                        SettingsRow(
                            title = "关于",
                            trailing = "v${AppInfo.VERSION}",
                            showChevron = true,
                            onClick = { aboutOpen = true },
                        )
                    }

                    Spacer(Modifier.height(24.dp))
                    GroupCard {
                        SettingsRow(
                            title = "退出登录",
                            danger = true,
                            onClick = onLogout,
                        )
                    }
                    Spacer(Modifier.height(28.dp))
                }
            }

            SnackbarHost(
                hostState = snackbar,
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(16.dp),
            )
        }
    }

    if (serverOpen) {
        IosDialog(
            title = "服务器",
            message = "填写要连接的服务器地址。保存后需要重新登录。",
            confirmText = "保存并退出",
            confirmEnabled = serverInput.isNotBlank(),
            onConfirm = {
                val normalized = Api.normalizeServerUrl(serverInput)
                if (normalized == null) {
                    serverError = "地址无效，例如 http://192.168.1.10:8787"
                } else {
                    serverOpen = false
                    if (normalized == prefs.serverUrl) {
                        scope.launch { snackbar.showSnackbar("服务器没有变化") }
                    } else {
                        prefs.serverUrl = normalized
                        onServerChanged(normalized)
                    }
                }
            },
            onDismiss = { serverOpen = false },
        ) {
            DialogTextField(
                value = serverInput,
                onValueChange = {
                    serverInput = it
                    serverError = ""
                },
                placeholder = Prefs.DEFAULT_SERVER,
            )
            if (serverError.isNotEmpty()) {
                Spacer(Modifier.height(8.dp))
                Text(
                    text = serverError,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                    fontSize = 13.sp,
                )
            }
        }
    }

    if (pairingOpen) {
        val lines = listOf(
            "本机 ID：${localId.ifBlank { "—（连接码会话）" }}",
            "服务器：${prefs.serverUrl}",
            "账号：${account.username}",
            "客户端：${AppInfo.NAME} v${AppInfo.VERSION}",
            "协议版本：1",
        )
        IosDialog(
            title = "本机配对信息",
            confirmText = "复制",
            dismissText = "关闭",
            onConfirm = {
                copy(lines.joinToString("\n"), "已复制本机配对信息")
                pairingOpen = false
            },
            onDismiss = { pairingOpen = false },
        ) {
            Column {
                lines.forEachIndexed { index, line ->
                    if (index > 0) Spacer(Modifier.height(8.dp))
                    Text(
                        text = line,
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (index == 0) {
                            MaterialTheme.colorScheme.onSurface
                        } else {
                            MaterialTheme.colorScheme.onSurfaceVariant
                        },
                        fontSize = 14.sp,
                    )
                }
            }
        }
    }

    if (aboutOpen) {
        IosDialog(
            title = "关于",
            confirmText = "复制反馈方式",
            dismissText = "好",
            onConfirm = {
                copy("QQ 群：128906393\n邮箱：xqtban@88.com", "已复制反馈方式")
                aboutOpen = false
            },
            onDismiss = { aboutOpen = false },
        ) {
            Column {
                Text(
                    text = "${AppInfo.NAME} v${AppInfo.VERSION}",
                    style = MaterialTheme.typography.bodyLarge,
                    fontSize = 16.sp,
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    text = "远程控制 ScreenWall 服务器的安卓主控端，只控制、不被控。",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
                Spacer(Modifier.height(14.dp))
                VersionLine("安卓端版本", "v${AppInfo.VERSION}")
                Spacer(Modifier.height(6.dp))
                VersionLine(
                    label = "服务端版本",
                    value = when {
                        serverVersionLoading -> "查询中…"
                        serverVersion != null -> "v$serverVersion"
                        else -> "暂不可用"
                    },
                )
                Spacer(Modifier.height(10.dp))
                RowDivider(startPadding = 0)
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .clickable {
                            val found = updateFound
                            if (found != null) openUrl(found.downloadUrl) else checkUpdates()
                        }
                        .padding(vertical = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(
                            text = "检查更新",
                            style = MaterialTheme.typography.bodyLarge,
                            fontSize = 15.sp,
                        )
                        Text(
                            text = updateMessage.ifEmpty {
                                if (checkingUpdate) "正在检查…" else "在服务器和 GitHub 上查找安卓安装包"
                            },
                            style = MaterialTheme.typography.bodyMedium,
                            color = if (updateMessage.startsWith("检查失败")) {
                                MaterialTheme.colorScheme.error
                            } else {
                                MaterialTheme.colorScheme.onSurfaceVariant
                            },
                            fontSize = 12.sp,
                        )
                    }
                    if (checkingUpdate) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(15.dp),
                            strokeWidth = 2.dp,
                            color = SW.accent,
                        )
                    } else {
                        Text(
                            text = if (updateFound != null) "去下载" else "检查",
                            color = SW.accent,
                            fontSize = 14.sp,
                            style = MaterialTheme.typography.labelLarge,
                        )
                    }
                }
                updateFound?.let { info ->
                    if (info.notes.isNotBlank()) {
                        Text(
                            text = info.notes.trim(),
                            maxLines = 5,
                            overflow = TextOverflow.Ellipsis,
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            fontSize = 12.sp,
                        )
                        Spacer(Modifier.height(8.dp))
                    }
                }
                Text(
                    text = "官方 QQ 群 128906393\n邮箱 xqtban@88.com\nGitHub kaynai1/screenwall",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
            }
        }
    }
}

@Composable
private fun VersionLine(label: String, value: String) {
    Row(Modifier.fillMaxWidth()) {
        Text(
            text = label,
            modifier = Modifier.weight(1f),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            fontSize = 13.sp,
        )
        Text(
            text = value,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurface,
            fontSize = 13.sp,
        )
    }
}

@Composable
private fun Caption(text: String) {
    Text(
        text = text,
        modifier = Modifier.padding(start = 4.dp, end = 4.dp, top = 8.dp),
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        fontSize = 13.sp,
    )
}

private fun serverLabel(url: String): String {
    if (url == Prefs.DEFAULT_SERVER) return "默认"
    return url
        .removePrefix("https://")
        .removePrefix("http://")
        .trimEnd('/')
}
