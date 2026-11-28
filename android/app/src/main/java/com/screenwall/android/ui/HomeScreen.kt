package com.screenwall.android.ui

import android.graphics.BitmapFactory
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
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Settings
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
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.Prefs
import com.screenwall.android.RecentDevice
import com.screenwall.android.data.Account
import com.screenwall.android.data.Api
import com.screenwall.android.data.Device
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
    SystemBars(lightIcons = androidx.compose.foundation.isSystemInDarkTheme().not())
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }

    var devices by remember { mutableStateOf<List<Device>>(emptyList()) }
    var recents by remember { mutableStateOf(prefs.recents()) }
    var connected by remember { mutableStateOf(session.viewer.isOpen) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf("") }

    var code by remember { mutableStateOf("") }
    var fieldError by remember { mutableStateOf("") }
    var dialogOpen by remember { mutableStateOf(false) }
    var password by remember { mutableStateOf("") }
    var dialogError by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }

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

    fun connectByCode() {
        val value = code.trim()
        fieldError = ""
        when {
            value.matches(Regex("\\d{9}")) -> {
                password = ""
                dialogError = ""
                dialogOpen = true
            }
            value.contains('.') || value.contains(':') -> {
                fieldError = "IP 直连暂不支持，请填被控端上的 9 位识别码"
            }
            else -> fieldError = "识别码是 9 位数字"
        }
    }

    fun submitQuick() {
        if (busy || password.isBlank()) return
        busy = true
        dialogError = ""
        scope.launch {
            try {
                val account = session.api.quickConnect(
                    session.account.serverUrl,
                    code.trim(),
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
                dialogOpen = false
                onQuickConnected(account, device)
            } catch (throwable: Throwable) {
                dialogError = describeThrow(throwable)
            } finally {
                busy = false
            }
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
                            value = code,
                            onValueChange = {
                                code = it
                                fieldError = ""
                            },
                            error = fieldError,
                            onConnect = { connectByCode() },
                        )
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

    if (dialogOpen) {
        IosDialog(
            title = "连接 ${code.trim()}",
            message = "输入被控端「连接码连接」里设置的密码",
            confirmText = if (busy) "连接中…" else "连接",
            confirmEnabled = password.isNotBlank() && !busy,
            onConfirm = { submitQuick() },
            onDismiss = { if (!busy) dialogOpen = false },
        ) {
            DialogTextField(
                value = password,
                onValueChange = {
                    password = it
                    dialogError = ""
                },
                placeholder = "连接密码",
                password = true,
                keyboardType = KeyboardType.Password,
                imeAction = ImeAction.Done,
                onImeAction = { submitQuick() },
            )
            if (dialogError.isNotEmpty()) {
                Spacer(Modifier.height(8.dp))
                Text(
                    text = dialogError,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
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
                Icon(
                    imageVector = Icons.Filled.CheckCircle,
                    contentDescription = null,
                    tint = color,
                    modifier = Modifier.size(16.dp),
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
                    connected -> "已连接服务器"
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
    value: String,
    onValueChange: (String) -> Unit,
    error: String,
    onConnect: () -> Unit,
) {
    GroupCard {
        Column(Modifier.padding(16.dp)) {
            Text(
                text = "远程桌面",
                style = MaterialTheme.typography.titleMedium,
                fontSize = 20.sp,
            )
            Spacer(Modifier.height(12.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier
                        .weight(1f)
                        .height(46.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(MaterialTheme.colorScheme.onSurface.copy(alpha = 0.06f))
                        .padding(horizontal = 12.dp),
                    contentAlignment = Alignment.CenterStart,
                ) {
                    BasicTextField(
                        value = value,
                        onValueChange = onValueChange,
                        singleLine = true,
                        textStyle = MaterialTheme.typography.bodyLarge.copy(
                            color = MaterialTheme.colorScheme.onSurface,
                            fontSize = 16.sp,
                        ),
                        cursorBrush = SolidColor(SW.accent),
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Text),
                        modifier = Modifier.fillMaxWidth(),
                        decorationBox = { inner ->
                            Box {
                                if (value.isEmpty()) {
                                    Text(
                                        text = "设备 ID 或 IP 地址",
                                        color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.7f),
                                        fontSize = 16.sp,
                                    )
                                }
                                inner()
                            }
                        },
                    )
                }
                Spacer(Modifier.width(10.dp))
                Box(
                    modifier = Modifier
                        .width(82.dp)
                        .height(46.dp)
                        .clip(RoundedCornerShape(12.dp))
                        .background(
                            if (value.isNotBlank()) SW.accent else SW.accent.copy(alpha = 0.35f),
                        )
                        .clickable(enabled = value.isNotBlank(), onClick = onConnect),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        text = "连接",
                        color = Color.White,
                        fontSize = 16.sp,
                        style = MaterialTheme.typography.labelLarge,
                    )
                }
            }
            if (error.isNotEmpty()) {
                Spacer(Modifier.height(8.dp))
                Text(
                    text = error,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                    fontSize = 13.sp,
                )
            }
        }
    }
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
