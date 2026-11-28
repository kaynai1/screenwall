package com.screenwall.android.ui

import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Download
import androidx.compose.material.icons.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Shield
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.Prefs
import com.screenwall.android.RecentDevice
import com.screenwall.android.data.Account
import com.screenwall.android.data.Api
import com.screenwall.android.data.AppInfo
import com.screenwall.android.data.AppUpdate
import com.screenwall.android.data.Device
import com.screenwall.android.data.Updates
import com.screenwall.android.data.describeThrow
import com.screenwall.android.net.ViewerClient
import com.screenwall.android.session.AppSession
import kotlinx.coroutines.launch
import org.json.JSONObject

/**
 * 首页：服务器状态 + 识别码直连 + 最近连接。
 *
 * 结构对齐 CrossDesk 那类手机端：不强迫先看设备列表，知道识别码就能直接连；
 * 常连的电脑留在「最近连接」，完整设备列表放在右上角「全部设备」。
 */
@Composable
fun HomeScreen(
    session: AppSession,
    prefs: Prefs,
    onOpenDevice: (Device) -> Unit,
    onOpenDevices: () -> Unit,
    onOpenSettings: () -> Unit,
    onQuickConnected: (Account, Device) -> Unit,
) {
    SystemBars(lightIcons = !LocalDarkTheme.current)
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }

    var devices by remember { mutableStateOf<List<Device>>(emptyList()) }
    var recents by remember { mutableStateOf(prefs.recents()) }
    var connected by remember { mutableStateOf(session.viewer.isOpen) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf("") }

    var code by remember { mutableStateOf("") }
    var password by remember { mutableStateOf("") }
    var fieldError by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }

    var update by remember { mutableStateOf<AppUpdate?>(null) }
    var updateOpen by remember { mutableStateOf(false) }
    val context = LocalContext.current

    suspend fun load() {
        try {
            if (!session.viewer.isOpen) {
                session.viewer.connect(session.account.serverUrl, session.account.token)
            }
            connected = true
            // 识别码令牌只能看一台设备，账号接口会拒绝；设备信息走 WS 推送。
            if (!session.account.quick) {
                devices = session.api.devices(session.account)
            }
            error = ""
        } catch (throwable: Throwable) {
            error = describeThrow(throwable)
            connected = session.viewer.isOpen
        } finally {
            loading = false
        }
    }

    DisposableEffect(Unit) {
        session.viewer.listener = object : ViewerClient.Listener {
            override fun onDevices(list: List<JSONObject>) {
                val parsed = list.map(Device::fromJson)
                scope.launch { devices = parsed }
            }

            override fun onSocketClosed(reason: String) {
                scope.launch {
                    connected = false
                    error = "连接已断开：$reason"
                }
            }
        }
        onDispose { }
    }

    LaunchedEffect(Unit) {
        recents = prefs.recents()
        load()
    }

    // 后台看一眼有没有新的安卓安装包；查不到（服务器没配置、GitHub 不通）就安静跳过。
    // 查到就顺手弹出对话框：只在首页横幅里放一行字，用户根本注意不到，
    // 等于没提醒。
    LaunchedEffect(Unit) {
        runCatching { Updates.check(session.http, session.account.serverUrl) }
            .getOrNull()
            ?.let {
                update = it
                updateOpen = true
            }
    }

    fun submitQuick() {
        if (busy) return
        val value = code.filter(Char::isDigit)
        if (value.length != 9 || password.isBlank()) return
        busy = true
        fieldError = ""
        scope.launch {
            try {
                val account = session.api.quickConnect(
                    session.account.serverUrl,
                    value,
                    password,
                )
                prefs.serverUrl = account.serverUrl
                val device = Device(
                    id = account.deviceId,
                    name = account.username,
                    os = "",
                    version = "",
                    online = true,
                    lastSeen = System.currentTimeMillis(),
                    controllable = true,
                )
                prefs.addRecent(device.id, device.name, device.os)
                recents = prefs.recents()
                password = ""
                onQuickConnected(account, device)
            } catch (throwable: Throwable) {
                fieldError = describeThrow(throwable)
            } finally {
                busy = false
            }
        }
    }

    fun connectByCode() {
        if (busy) return
        val value = code.filter(Char::isDigit)
        fieldError = ""
        when {
            value.length != 9 -> {
                fieldError = if (code.contains('.') || code.contains(':')) {
                    "IP 直连暂不支持，请填被控端上的 9 位识别码"
                } else {
                    "识别码是 9 位数字"
                }
            }
            password.isBlank() -> fieldError = "请输入连接密码"
            else -> submitQuick()
        }
    }

    fun openRecent(recent: RecentDevice) {
        val known = devices.firstOrNull { it.id == recent.id }
        if (session.account.quick && recent.id == session.account.deviceId) {
            onOpenDevice(
                known ?: Device(recent.id, recent.name, recent.os, "", true, recent.lastAt, true),
            )
            return
        }
        if (known == null) {
            scope.launch {
                snackbar.showSnackbar(
                    if (!loading && !session.account.quick) "这台设备不在线或已被移除" else "设备信息还没加载好",
                )
            }
            return
        }
        if (!known.online) {
            scope.launch { snackbar.showSnackbar("这台设备现在不在线") }
            return
        }
        if (!known.controllable) {
            scope.launch { snackbar.showSnackbar("这台电脑关闭了「允许被控」") }
            return
        }
        onOpenDevice(known)
    }

    ScreenBackground {
        Box(Modifier.fillMaxSize()) {
            Column(Modifier.fillMaxSize().statusBarsPadding()) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(start = 20.dp, end = 8.dp, top = 10.dp, bottom = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    StatusPill(
                        connected = connected,
                        loading = loading,
                        onClick = { scope.launch { load() } },
                    )
                    Spacer(Modifier.weight(1f))
                    IconButton(onClick = onOpenSettings, modifier = Modifier.size(44.dp)) {
                        Icon(
                            imageVector = Icons.Filled.Settings,
                            contentDescription = "设置",
                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }

                LazyColumn(
                    modifier = Modifier.fillMaxSize(),
                    contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 28.dp),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    item {
                        RemoteDesktopCard(
                            code = code,
                            onCodeChange = {
                                code = it
                                fieldError = ""
                            },
                            password = password,
                            onPasswordChange = {
                                password = it
                                fieldError = ""
                            },
                            error = fieldError,
                            busy = busy,
                            onConnect = { connectByCode() },
                        )
                    }
                    update?.let { info ->
                        item {
                            UpdateBanner(info = info, onClick = { updateOpen = true })
                        }
                    }
                    if (error.isNotEmpty() && !connected) {
                        item {
                            Text(
                                text = error,
                                color = MaterialTheme.colorScheme.error,
                                style = MaterialTheme.typography.bodyMedium,
                                modifier = Modifier.padding(horizontal = 4.dp),
                            )
                        }
                    }
                    item {
                        RecentsHeader(
                            count = recents.size,
                            onOpenDevices = onOpenDevices,
                        )
                    }
                    if (recents.isEmpty()) {
                        item { EmptyRecents() }
                    } else {
                        item {
                            LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                                items(recents, key = { it.id }) { recent ->
                                    val known = devices.firstOrNull { it.id == recent.id }
                                    val online = when {
                                        session.account.quick && recent.id == session.account.deviceId -> true
                                        else -> known?.online == true
                                    }
                                    RecentCard(
                                        recent = recent,
                                        online = online,
                                        prefs = prefs,
                                        onClick = { openRecent(recent) },
                                    )
                                }
                            }
                        }
                    }
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

    val pending = update
    if (updateOpen && pending != null) {
        IosDialog(
            title = "发现新版本 ${Updates.pretty(pending.version)}",
            message = "当前是 v${AppInfo.VERSION}，最新版是 ${Updates.pretty(pending.version)}。",
            confirmText = "去下载",
            dismissText = "稍后",
            onConfirm = {
                updateOpen = false
                runCatching {
                    context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(pending.downloadUrl)))
                }
            },
            onDismiss = { updateOpen = false },
        ) {
            if (pending.notes.isNotBlank()) {
                Text(
                    text = pending.notes.trim(),
                    maxLines = 8,
                    overflow = TextOverflow.Ellipsis,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
            }
        }
    }
}

@Composable
private fun StatusPill(connected: Boolean, loading: Boolean, onClick: () -> Unit) {
    val color = when {
        connected -> SW.green
        loading -> SW.orange
        else -> SW.red
    }
    Surface(
        modifier = Modifier
            .clip(CircleShape)
            .clickable(onClick = onClick),
        shape = CircleShape,
        color = color.copy(alpha = 0.14f),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 7.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (connected) {
                // 和 Windows 端的链路胶囊一样：一个状态点 + 当前协议。
                Box(
                    modifier = Modifier
                        .size(6.dp)
                        .clip(CircleShape)
                        .background(color),
                )
            } else {
                CircularProgressIndicator(
                    modifier = Modifier.size(14.dp),
                    strokeWidth = 2.dp,
                    color = color,
                )
            }
            Spacer(Modifier.width(6.dp))
            Text(
                text = when {
                    connected -> "已连接服务器 · 中转"
                    loading -> "正在连接服务器…"
                    else -> "未连接服务器"
                },
                color = color,
                fontSize = 13.sp,
                style = MaterialTheme.typography.labelMedium,
            )
        }
    }
}

@Composable
private fun RemoteDesktopCard(
    code: String,
    onCodeChange: (String) -> Unit,
    password: String,
    onPasswordChange: (String) -> Unit,
    error: String,
    busy: Boolean,
    onConnect: () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    var showPassword by remember { mutableStateOf(false) }
    val canConnect = code.filter(Char::isDigit).length == 9 && password.isNotBlank() && !busy
    GroupCard {
        Column(Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier
                        .size(38.dp)
                        .clip(CircleShape)
                        .background(SW.accent.copy(alpha = 0.12f)),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Filled.Link,
                        contentDescription = null,
                        tint = SW.accent,
                        modifier = Modifier.size(20.dp),
                    )
                }
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        text = "控制别人的电脑",
                        style = MaterialTheme.typography.titleMedium,
                        fontSize = 17.sp,
                    )
                    Text(
                        text = "填对方的识别码和密码",
                        style = MaterialTheme.typography.bodyMedium,
                        color = colors.onSurfaceVariant,
                        fontSize = 13.sp,
                    )
                }
            }
            Spacer(Modifier.height(16.dp))

            FieldLabel("对方的识别码")
            Spacer(Modifier.height(6.dp))
            CodeInputBox(
                value = code,
                onValueChange = { raw -> onCodeChange(raw.filter(Char::isDigit).take(9)) },
                placeholder = "9 位识别码",
                keyboardType = KeyboardType.Number,
                imeAction = ImeAction.Next,
            )
            Spacer(Modifier.height(12.dp))

            FieldLabel("密码")
            Spacer(Modifier.height(6.dp))
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(46.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .background(colors.onSurface.copy(alpha = 0.06f))
                    .padding(horizontal = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                BasicTextField(
                    value = password,
                    onValueChange = onPasswordChange,
                    singleLine = true,
                    textStyle = MaterialTheme.typography.bodyLarge.copy(
                        color = colors.onSurface,
                        fontSize = 16.sp,
                    ),
                    cursorBrush = SolidColor(SW.accent),
                    visualTransformation = if (showPassword) {
                        VisualTransformation.None
                    } else {
                        PasswordVisualTransformation()
                    },
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Password,
                        imeAction = ImeAction.Done,
                    ),
                    keyboardActions = KeyboardActions(
                        onDone = { onConnect() },
                        onGo = { onConnect() },
                    ),
                    modifier = Modifier.weight(1f),
                    decorationBox = { inner ->
                        Box(contentAlignment = Alignment.CenterStart) {
                            if (password.isEmpty()) {
                                Text(
                                    text = "临时密码或固定密码",
                                    color = colors.onSurfaceVariant.copy(alpha = 0.7f),
                                    fontSize = 16.sp,
                                )
                            }
                            inner()
                        }
                    },
                )
                Text(
                    text = if (showPassword) "隐藏" else "显示",
                    color = SW.accent,
                    fontSize = 14.sp,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .clickable { showPassword = !showPassword }
                        .padding(horizontal = 8.dp, vertical = 6.dp),
                )
            }
            Spacer(Modifier.height(14.dp))
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(48.dp)
                    .clip(RoundedCornerShape(14.dp))
                    .background(if (canConnect) SW.accent else SW.accent.copy(alpha = 0.35f))
                    .clickable(enabled = canConnect, onClick = onConnect),
                horizontalArrangement = Arrangement.Center,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (busy) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(16.dp),
                        color = Color.White,
                        strokeWidth = 2.dp,
                    )
                } else {
                    Icon(
                        imageVector = Icons.Filled.Link,
                        contentDescription = null,
                        tint = Color.White,
                        modifier = Modifier.size(17.dp),
                    )
                }
                Spacer(Modifier.width(8.dp))
                Text(
                    text = if (busy) "正在连接…" else "连接",
                    color = Color.White,
                    fontSize = 16.sp,
                    style = MaterialTheme.typography.labelLarge,
                )
            }
            if (error.isNotEmpty()) {
                Spacer(Modifier.height(10.dp))
                Text(
                    text = error,
                    color = colors.error,
                    style = MaterialTheme.typography.bodyMedium,
                    fontSize = 13.sp,
                )
            }
            Spacer(Modifier.height(12.dp))
            Row(verticalAlignment = Alignment.Top) {
                Icon(
                    imageVector = Icons.Filled.Shield,
                    contentDescription = null,
                    tint = colors.onSurfaceVariant,
                    modifier = Modifier.size(14.dp),
                )
                Spacer(Modifier.width(6.dp))
                Text(
                    text = "识别码和密码在对方「远程协助」页顶部；同一网段会自动直连。",
                    style = MaterialTheme.typography.bodyMedium,
                    color = colors.onSurfaceVariant,
                    fontSize = 12.sp,
                )
            }
        }
    }
}

@Composable
private fun UpdateBanner(info: AppUpdate, onClick: () -> Unit) {
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(14.dp))
            .clickable(onClick = onClick),
        shape = RoundedCornerShape(14.dp),
        color = SW.accent.copy(alpha = 0.10f),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 11.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                imageVector = Icons.Filled.Download,
                contentDescription = null,
                tint = SW.accent,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(10.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    text = "发现新版本 ${Updates.pretty(info.version)}",
                    color = SW.accent,
                    fontSize = 14.sp,
                    style = MaterialTheme.typography.labelLarge,
                )
                Text(
                    text = "点这里查看更新说明并下载",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 12.sp,
                )
            }
            Icon(
                imageVector = Icons.Filled.KeyboardArrowRight,
                contentDescription = null,
                tint = SW.accent,
                modifier = Modifier.size(18.dp),
            )
        }
    }
}

@Composable
private fun FieldLabel(text: String) {
    Text(
        text = text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        fontSize = 13.sp,
    )
}

@Composable
private fun CodeInputBox(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    keyboardType: KeyboardType,
    imeAction: ImeAction,
) {
    val colors = MaterialTheme.colorScheme
    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        singleLine = true,
        textStyle = MaterialTheme.typography.bodyLarge.copy(
            color = colors.onSurface,
            fontSize = 17.sp,
            letterSpacing = 1.sp,
        ),
        cursorBrush = SolidColor(SW.accent),
        keyboardOptions = KeyboardOptions(keyboardType = keyboardType, imeAction = imeAction),
        modifier = Modifier
            .fillMaxWidth()
            .height(46.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(colors.onSurface.copy(alpha = 0.06f))
            .padding(horizontal = 12.dp),
        decorationBox = { inner ->
            Box(contentAlignment = Alignment.CenterStart) {
                if (value.isEmpty()) {
                    Text(
                        text = placeholder,
                        color = colors.onSurfaceVariant.copy(alpha = 0.7f),
                        fontSize = 16.sp,
                    )
                }
                inner()
            }
        },
    )
}

@Composable
private fun RecentsHeader(count: Int, onOpenDevices: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 10.dp, start = 4.dp, end = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = "最近连接",
            style = MaterialTheme.typography.titleMedium,
            fontSize = 20.sp,
        )
        Spacer(Modifier.weight(1f))
        Text(
            text = "$count 台设备",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            fontSize = 13.sp,
        )
        Spacer(Modifier.width(12.dp))
        Row(
            modifier = Modifier
                .clip(RoundedCornerShape(8.dp))
                .clickable(onClick = onOpenDevices)
                .padding(start = 4.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "全部设备",
                color = SW.accent,
                fontSize = 15.sp,
                style = MaterialTheme.typography.labelLarge,
            )
            Icon(
                imageVector = Icons.Filled.KeyboardArrowRight,
                contentDescription = null,
                tint = SW.accent,
                modifier = Modifier.size(18.dp),
            )
        }
    }
}

@Composable
private fun EmptyRecents() {
    GroupCard {
        Text(
            text = "连接过的电脑会出现在这里。",
            modifier = Modifier.padding(16.dp),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun RecentCard(
    recent: RecentDevice,
    online: Boolean,
    prefs: Prefs,
    onClick: () -> Unit,
) {
    val image = remember(recent.id, recent.lastAt) {
        prefs.previewFile(recent.id)
            ?.takeIf { it.exists() }
            ?.let { file ->
                runCatching {
                    BitmapFactory.decodeFile(file.absolutePath)?.asImageBitmap()
                }.getOrNull()
            }
    }
    Column(
        modifier = Modifier
            .width(190.dp)
            .clip(RoundedCornerShape(18.dp))
            .background(MaterialTheme.colorScheme.surface)
            .clickable(onClick = onClick),
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(104.dp)
                .background(osBrush(recent.os)),
            contentAlignment = Alignment.Center,
        ) {
            if (image != null) {
                Image(
                    bitmap = image,
                    contentDescription = null,
                    modifier = Modifier.fillMaxSize(),
                    contentScale = ContentScale.Crop,
                )
            } else {
                Text(
                    text = osLabel(recent.os),
                    color = Color.White,
                    fontSize = 21.sp,
                    style = MaterialTheme.typography.titleMedium,
                )
            }
        }
        Column(Modifier.padding(horizontal = 12.dp, vertical = 10.dp)) {
            Text(
                text = recent.name,
                style = MaterialTheme.typography.titleMedium,
                fontSize = 15.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(4.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    text = recent.id,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 12.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Spacer(Modifier.width(6.dp))
                Box(
                    modifier = Modifier
                        .size(6.dp)
                        .clip(CircleShape)
                        .background(if (online) SW.green else MaterialTheme.colorScheme.outline),
                )
                Spacer(Modifier.width(4.dp))
                Text(
                    text = if (online) "在线" else "离线",
                    color = if (online) SW.green else MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 12.sp,
                )
            }
        }
    }
}

private fun osLabel(os: String): String {
    val value = os.trim().lowercase()
    return when {
        value.startsWith("win") -> "Windows"
        value.contains("mac") -> "macOS"
        value.contains("linux") -> "Linux"
        value.startsWith("android") -> "Android"
        value.startsWith("web") -> "Web"
        value.isBlank() -> "远程电脑"
        else -> os
    }
}

private fun osBrush(os: String): Brush {
    val value = os.trim().lowercase()
    return when {
        value.startsWith("win") -> Brush.linearGradient(listOf(Color(0xFF2F6FE0), Color(0xFF1B4AA8)))
        value.contains("mac") -> Brush.linearGradient(listOf(Color(0xFF9A9AA0), Color(0xFF5C5C63)))
        value.contains("linux") -> Brush.linearGradient(listOf(Color(0xFFE0912F), Color(0xFFB4650F)))
        value.startsWith("android") -> Brush.linearGradient(listOf(Color(0xFF3DDC84), Color(0xFF1E7F46)))
        else -> Brush.linearGradient(listOf(Color(0xFF41A0FF), Color(0xFF0055C4)))
    }
}
