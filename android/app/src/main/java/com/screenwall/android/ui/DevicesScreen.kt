package com.screenwall.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
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
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.ExperimentalMaterialApi
import androidx.compose.material.pullrefresh.PullRefreshIndicator
import androidx.compose.material.pullrefresh.pullRefresh
import androidx.compose.material.pullrefresh.rememberPullRefreshState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Logout
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
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.data.Device
import com.screenwall.android.data.describeThrow
import com.screenwall.android.net.ViewerClient
import com.screenwall.android.session.AppSession
import kotlinx.coroutines.launch
import org.json.JSONObject
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@OptIn(ExperimentalMaterialApi::class)
@Composable
fun DevicesScreen(
    session: AppSession,
    onOpenDevice: (Device) -> Unit,
    onLogout: () -> Unit,
) {
    SystemBars(lightIcons = !isSystemInDarkTheme())
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    var devices by remember { mutableStateOf<List<Device>>(emptyList()) }
    var refreshing by remember { mutableStateOf(false) }
    var loading by remember { mutableStateOf(true) }
    var error by remember { mutableStateOf("") }
    var connected by remember { mutableStateOf(session.viewer.isOpen) }

    suspend fun load(showSpinner: Boolean) {
        if (showSpinner) refreshing = true
        try {
            if (!session.viewer.isOpen) {
                session.viewer.connect(session.account.serverUrl, session.account.token)
            }
            connected = true
            devices = session.api.devices(session.account)
            error = ""
        } catch (throwable: Throwable) {
            error = describeThrow(throwable)
            connected = session.viewer.isOpen
        } finally {
            refreshing = false
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
                    if (error.isEmpty()) error = "连接已断开：$reason"
                }
            }
        }
        onDispose { }
    }

    LaunchedEffect(Unit) {
        load(showSpinner = false)
    }

    val pullState = rememberPullRefreshState(
        refreshing = refreshing,
        onRefresh = { scope.launch { load(showSpinner = true) } },
    )

    ScreenBackground {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .statusBarsPadding(),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 20.dp, end = 8.dp, top = 10.dp, bottom = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(
                        text = "设备",
                        style = MaterialTheme.typography.headlineLarge,
                    )
                    Text(
                        text = "账号 · ${session.account.username}",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                IconButton(onClick = onLogout, modifier = Modifier.size(44.dp)) {
                    Icon(
                        imageVector = Icons.Filled.Logout,
                        contentDescription = "退出登录",
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }

            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .pullRefresh(pullState),
            ) {
                LazyColumn(
                    modifier = Modifier.fillMaxSize(),
                    contentPadding = androidx.compose.foundation.layout.PaddingValues(
                        start = 16.dp,
                        end = 16.dp,
                        top = 8.dp,
                        bottom = 28.dp,
                    ),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    if (!connected) {
                        item { StatusBanner(text = error.ifBlank { "正在连接服务器…" }) }
                    }
                    if (loading) {
                        item {
                            Box(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(top = 80.dp),
                                contentAlignment = Alignment.Center,
                            ) {
                                CircularProgressIndicator(color = SW.accent, strokeWidth = 2.dp)
                            }
                        }
                    } else if (devices.isEmpty()) {
                        item { EmptyDevices() }
                    } else {
                        items(devices, key = { it.id }) { device ->
                            DeviceCard(
                                device = device,
                                onClick = {
                                    if (!device.online) {
                                        scope.launch { snackbar.showSnackbar("这台设备现在不在线") }
                                    } else if (!device.controllable) {
                                        scope.launch { snackbar.showSnackbar("这台电脑关闭了「允许被控」") }
                                    } else {
                                        onOpenDevice(device)
                                    }
                                },
                            )
                        }
                        if (error.isNotEmpty() && connected) {
                            item {
                                Text(
                                    text = error,
                                    color = MaterialTheme.colorScheme.error,
                                    style = MaterialTheme.typography.bodyMedium,
                                    modifier = Modifier.padding(horizontal = 4.dp),
                                )
                            }
                        }
                    }
                }

                PullRefreshIndicator(
                    refreshing = refreshing,
                    state = pullState,
                    modifier = Modifier.align(Alignment.TopCenter),
                    backgroundColor = MaterialTheme.colorScheme.surface,
                    contentColor = SW.accent,
                )
                SnackbarHost(
                    hostState = snackbar,
                    modifier = Modifier
                        .align(Alignment.BottomCenter)
                        .padding(16.dp),
                )
            }
        }
    }
}

@Composable
private fun DeviceCard(device: Device, onClick: () -> Unit) {
    Surface(
        modifier = Modifier
            .fillMaxWidth()
            .alpha(if (device.online) 1f else 0.62f)
            .clip(RoundedCornerShape(20.dp))
            .clickable(onClick = onClick),
        shape = RoundedCornerShape(20.dp),
        color = MaterialTheme.colorScheme.surface,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 15.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(46.dp)
                    .clip(RoundedCornerShape(14.dp))
                    .background(SW.accent.copy(alpha = 0.12f)),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = osBadge(device.os),
                    color = SW.accent,
                    fontSize = 14.sp,
                    style = MaterialTheme.typography.labelMedium,
                )
            }
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    text = device.name,
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(5.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier = Modifier
                            .size(8.dp)
                            .clip(CircleShape)
                            .background(if (device.online) SW.green else MaterialTheme.colorScheme.outline),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = lastSeenText(device),
                        style = MaterialTheme.typography.bodyMedium,
                        color = if (device.online) SW.green else MaterialTheme.colorScheme.onSurfaceVariant,
                        fontSize = 13.sp,
                    )
                }
                Spacer(Modifier.height(3.dp))
                Text(
                    text = deviceDetail(device),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Icon(
                imageVector = Icons.Filled.KeyboardArrowRight,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

@Composable
private fun StatusBanner(text: String) {
    Surface(
        shape = RoundedCornerShape(14.dp),
        color = MaterialTheme.colorScheme.surface,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(16.dp),
                strokeWidth = 2.dp,
                color = SW.accent,
            )
            Spacer(Modifier.width(10.dp))
            Text(
                text = text,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun EmptyDevices() {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 72.dp, start = 24.dp, end = 24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(
            text = "还没有在线的设备",
            style = MaterialTheme.typography.titleMedium,
        )
        Spacer(Modifier.height(8.dp))
        Text(
            text = "在要控制的电脑上登录同一个账号，并保持被控端运行。",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
        )
    }
}

private fun osBadge(os: String): String {
    val value = os.trim()
    return when {
        value.startsWith("win", ignoreCase = true) -> "Win"
        value.contains("mac", ignoreCase = true) -> "Mac"
        value.contains("linux", ignoreCase = true) -> "Linux"
        value.contains("android", ignoreCase = true) -> "安卓"
        value.contains("web", ignoreCase = true) -> "Web"
        value.isBlank() -> "PC"
        else -> value.take(3)
    }
}

private fun deviceDetail(device: Device): String {
    val parts = mutableListOf<String>()
    if (device.os.isNotBlank()) parts.add(device.os)
    if (device.version.isNotBlank()) parts.add("v${device.version}")
    if (!device.controllable) parts.add("已关闭被控")
    return if (parts.isEmpty()) "ScreenWall 设备" else parts.joinToString(" · ")
}

private fun lastSeenText(device: Device): String {
    if (device.online) return "在线"
    if (device.lastSeen <= 0) return "从未上线"
    val diff = System.currentTimeMillis() - device.lastSeen
    return when {
        diff < 60_000 -> "刚刚在线"
        diff < 3_600_000 -> "${diff / 60_000} 分钟前在线"
        diff < 86_400_000 -> "${diff / 3_600_000} 小时前在线"
        diff < 7 * 86_400_000L -> "${diff / 86_400_000} 天前在线"
        else -> SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()).format(Date(device.lastSeen))
    }
}
