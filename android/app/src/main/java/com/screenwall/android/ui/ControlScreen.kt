package com.screenwall.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ArrowBack
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material.icons.filled.VolumeOff
import androidx.compose.material.icons.filled.VolumeUp
import androidx.compose.material3.BottomSheetDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
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
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.Prefs
import com.screenwall.android.data.Device
import com.screenwall.android.session.AppSession
import com.screenwall.android.session.ControlController
import com.screenwall.android.session.DisplayInfo
import com.screenwall.android.session.InputMode
import com.screenwall.android.session.Phase
import com.screenwall.android.session.SessionStats
import com.screenwall.android.session.StreamOptions
import kotlinx.coroutines.launch
import kotlin.math.roundToInt

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ControlScreen(
    session: AppSession,
    device: Device,
    prefs: Prefs,
    onBack: () -> Unit,
) {
    val controller = remember {
        ControlController(session.viewer, session.account, device, prefs)
    }
    DisposableEffect(controller) {
        controller.start()
        onDispose { controller.dispose() }
    }
    KeepScreenOn()
    SystemBars(lightIcons = false)

    val phase by controller.phase.collectAsState()
    val errorText by controller.errorText.collectAsState()
    val stats by controller.stats.collectAsState()
    val audioOn by controller.audioOn.collectAsState()
    val inputMode by controller.inputMode.collectAsState()
    val options by controller.options.collectAsState()
    val displays by controller.displays.collectAsState()
    val displayIndex by controller.displayIndex.collectAsState()
    val audioStatus by controller.audioStatus.collectAsState()

    var settingsOpen by remember { mutableStateOf(false) }
    var keyboardOpen by remember { mutableStateOf(false) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()

    LaunchedEffect(Unit) {
        controller.notices.collect { snackbar.showSnackbar(it) }
    }

    BackHandler {
        when {
            keyboardOpen -> keyboardOpen = false
            settingsOpen -> settingsOpen = false
            else -> onBack()
        }
    }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
    ) {
        VideoStage(controller = controller, modifier = Modifier.fillMaxSize())

        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(132.dp)
                .align(Alignment.TopCenter)
                .background(
                    Brush.verticalGradient(
                        listOf(Color.Black.copy(alpha = 0.62f), Color.Transparent),
                    ),
                ),
        )

        Row(
            modifier = Modifier
                .align(Alignment.TopStart)
                .fillMaxWidth()
                .statusBarsPadding()
                .padding(horizontal = 12.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CircleIconButton(
                icon = Icons.Filled.ArrowBack,
                contentDescription = "返回",
                onClick = onBack,
            )
            Column(
                modifier = Modifier
                    .weight(1f)
                    .padding(horizontal = 10.dp),
            ) {
                Text(
                    text = device.name,
                    color = Color.White,
                    fontSize = 16.sp,
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(4.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    StatPill(text = statusText(stats))
                    val detail = detailText(stats)
                    if (detail.isNotEmpty()) StatPill(text = detail)
                }
            }
            CircleIconButton(
                icon = Icons.Filled.Tune,
                contentDescription = "会话设置",
                onClick = { settingsOpen = true },
            )
        }

        if (phase != Phase.Streaming) {
            PhaseOverlay(
                phase = phase,
                errorText = errorText,
                onRetry = { controller.retry() },
                onBack = onBack,
                modifier = Modifier.align(Alignment.Center),
            )
        }

        Column(
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .navigationBarsPadding()
                .imePadding()
                .padding(start = 16.dp, end = 16.dp, bottom = 10.dp),
        ) {
            if (keyboardOpen) {
                KeyboardPanel(
                    controller = controller,
                    onClose = { keyboardOpen = false },
                )
                Spacer(Modifier.height(8.dp))
            }
            Dock(
                audioOn = audioOn,
                onKeyboard = { keyboardOpen = !keyboardOpen },
                onAudio = { controller.toggleAudio() },
                onSettings = { settingsOpen = true },
                onDisconnect = onBack,
            )
        }

        SnackbarHost(
            hostState = snackbar,
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(bottom = 108.dp, start = 16.dp, end = 16.dp),
        )
    }

    if (settingsOpen) {
        ModalBottomSheet(
            onDismissRequest = { settingsOpen = false },
            containerColor = MaterialTheme.colorScheme.background,
            dragHandle = { BottomSheetDefaults.DragHandle() },
        ) {
            SettingsSheet(
                options = options,
                displays = displays,
                displayIndex = displayIndex,
                inputMode = inputMode,
                audioOn = audioOn,
                audioStatus = audioStatus,
                stats = stats,
                onOptions = { controller.updateOptions(it) },
                onInputMode = { controller.setInputMode(it) },
                onDisplay = { controller.selectDisplay(it) },
                onAudio = { controller.toggleAudio() },
                onDisconnect = {
                    settingsOpen = false
                    onBack()
                },
            )
        }
    }

    LaunchedEffect(settingsOpen) {
        if (settingsOpen) {
            snackbar.currentSnackbarData?.dismiss()
        }
    }
}

@Composable
private fun Dock(
    audioOn: Boolean,
    onKeyboard: () -> Unit,
    onAudio: () -> Unit,
    onSettings: () -> Unit,
    onDisconnect: () -> Unit,
) {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(22.dp),
        color = Color(0xF01C1C1E),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 6.dp, vertical = 6.dp),
            horizontalArrangement = Arrangement.SpaceEvenly,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            DockItem(Icons.Filled.Keyboard, "键盘", onClick = onKeyboard)
            DockItem(
                icon = if (audioOn) Icons.Filled.VolumeUp else Icons.Filled.VolumeOff,
                label = "声音",
                active = audioOn,
                onClick = onAudio,
            )
            DockItem(Icons.Filled.Tune, "设置", onClick = onSettings)
            DockItem(Icons.Filled.PowerSettingsNew, "断开", onClick = onDisconnect)
        }
    }
}

@Composable
private fun DockItem(
    icon: ImageVector,
    label: String,
    active: Boolean = false,
    onClick: () -> Unit,
) {
    val tint = if (active) SW.accent else Color.White
    Column(
        modifier = Modifier
            .width(68.dp)
            .heightIn(min = 54.dp)
            .clip(RoundedCornerShape(14.dp))
            .clickable(onClick = onClick)
            .padding(vertical = 6.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Icon(
            imageVector = icon,
            contentDescription = label,
            tint = tint,
            modifier = Modifier.size(22.dp),
        )
        Spacer(Modifier.height(3.dp))
        Text(
            text = label,
            color = tint.copy(alpha = 0.9f),
            fontSize = 11.sp,
        )
    }
}

@Composable
private fun PhaseOverlay(
    phase: Phase,
    errorText: String,
    onRetry: () -> Unit,
    onBack: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Box(
        modifier = modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = if (phase == Phase.Error) 0.72f else 0.5f)),
        contentAlignment = Alignment.Center,
    ) {
        if (phase == Phase.Error) {
            Surface(
                shape = RoundedCornerShape(20.dp),
                color = Color(0xF21C1C1E),
                modifier = Modifier.padding(horizontal = 28.dp),
            ) {
                Column(
                    modifier = Modifier.padding(20.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text(
                        text = "连不上这台设备",
                        color = Color.White,
                        style = MaterialTheme.typography.titleMedium,
                    )
                    Spacer(Modifier.height(8.dp))
                    Text(
                        text = errorText.ifBlank { "会话已断开" },
                        color = Color.White.copy(alpha = 0.7f),
                        fontSize = 14.sp,
                    )
                    Spacer(Modifier.height(18.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        SheetButton(text = "重试", filled = true, onClick = onRetry)
                        SheetButton(text = "返回", filled = false, onClick = onBack)
                    }
                }
            }
        } else {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator(color = Color.White, strokeWidth = 2.dp)
                Spacer(Modifier.height(14.dp))
                Text(
                    text = if (phase == Phase.Connecting) "正在请求连接…" else "等待被控端确认 / 首帧…",
                    color = Color.White.copy(alpha = 0.85f),
                    fontSize = 14.sp,
                )
            }
        }
    }
}

@Composable
private fun SheetButton(text: String, filled: Boolean, onClick: () -> Unit) {
    Box(
        modifier = Modifier
            .clip(RoundedCornerShape(12.dp))
            .background(if (filled) SW.accent else Color.White.copy(alpha = 0.14f))
            .clickable(onClick = onClick)
            .padding(horizontal = 18.dp, vertical = 11.dp),
    ) {
        Text(
            text = text,
            color = Color.White,
            fontSize = 15.sp,
            style = MaterialTheme.typography.labelLarge,
        )
    }
}

@Composable
private fun SettingsSheet(
    options: StreamOptions,
    displays: List<DisplayInfo>,
    displayIndex: Int,
    inputMode: InputMode,
    audioOn: Boolean,
    audioStatus: String,
    stats: SessionStats,
    onOptions: (StreamOptions) -> Unit,
    onInputMode: (InputMode) -> Unit,
    onDisplay: (Int) -> Unit,
    onAudio: () -> Unit,
    onDisconnect: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .verticalScroll(rememberScrollState())
            .padding(start = 20.dp, end = 20.dp, bottom = 28.dp),
    ) {
        Text(
            text = "会话设置",
            style = MaterialTheme.typography.titleLarge,
            modifier = Modifier.padding(bottom = 4.dp),
        )
        Text(
            text = buildString {
                append(stats.codec.ifBlank { "连接的画面" })
                if (stats.width > 0) append(" · ${stats.width}×${stats.height}")
                if (stats.kbps > 0) append(" · ${stats.kbps} kbps")
            },
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )

        SectionTitle("画质")
        GroupCard {
            Box(Modifier.padding(12.dp)) {
                SegmentedControl(
                    options = listOf(45 to "省流量", 65 to "均衡", 80 to "清晰"),
                    selected = options.quality,
                    onSelect = { onOptions(options.copy(quality = it)) },
                )
            }
        }

        SectionTitle("分辨率")
        GroupCard {
            Box(Modifier.padding(12.dp)) {
                SegmentedControl(
                    options = listOf(1280 to "标清", 1920 to "高清", 2560 to "2K", 3840 to "4K"),
                    selected = options.maxWidth,
                    onSelect = { onOptions(options.copy(maxWidth = it)) },
                )
            }
        }

        SectionTitle("帧率")
        GroupCard {
            Box(Modifier.padding(12.dp)) {
                SegmentedControl(
                    options = listOf(15 to "15 FPS", 30 to "30 FPS", 60 to "60 FPS"),
                    selected = options.fps,
                    onSelect = { onOptions(options.copy(fps = it)) },
                )
            }
        }

        SectionTitle("编码")
        GroupCard {
            SwitchRow(
                title = "优先使用 H.264 硬解",
                subtitle = "被控端没有硬件编码器时自动回到 MJPEG",
                checked = options.preferH264,
                onChange = { onOptions(options.copy(preferH264 = it)) },
            )
        }

        SectionTitle("输入")
        GroupCard {
            Box(Modifier.padding(12.dp)) {
                SegmentedControl(
                    options = listOf(InputMode.Absolute to "绝对位置", InputMode.Trackpad to "触控板"),
                    selected = inputMode,
                    onSelect = onInputMode,
                )
            }
            Text(
                text = if (inputMode == InputMode.Absolute) {
                    "手指点哪里，远端光标就去哪里；拖动等于按住左键移动。"
                } else {
                    "手指滑动移动光标，轻点发送左键；拖动文件请切到绝对位置。"
                },
                modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 14.dp),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontSize = 13.sp,
            )
        }

        SectionTitle("显示器")
        GroupCard {
            if (displays.isEmpty()) {
                Text(
                    text = "等待被控端上报显示器…",
                    modifier = Modifier.padding(16.dp),
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                displays.forEachIndexed { index, display ->
                    if (index > 0) RowDivider()
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(min = 52.dp)
                            .clickable { onDisplay(display.index) }
                            .padding(horizontal = 16.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                text = "显示器 ${display.index + 1}${if (display.primary) "（主屏）" else ""}",
                                style = MaterialTheme.typography.bodyLarge,
                                fontSize = 16.sp,
                            )
                            Text(
                                text = "${display.width}×${display.height}",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                fontSize = 13.sp,
                            )
                        }
                        if (display.index == displayIndex) {
                            Icon(
                                imageVector = Icons.Filled.Check,
                                contentDescription = "当前",
                                tint = SW.accent,
                            )
                        }
                    }
                }
            }
        }

        SectionTitle("声音")
        GroupCard {
            SwitchRow(
                title = "播放远端声音",
                subtitle = audioStatus,
                checked = audioOn,
                onChange = { onAudio() },
            )
            Text(
                text = "Opus 优先，设备不支持时用 PCM16。",
                modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 14.dp),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                fontSize = 13.sp,
            )
        }

        Spacer(Modifier.height(24.dp))
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 50.dp)
                .clip(RoundedCornerShape(14.dp))
                .background(MaterialTheme.colorScheme.surface)
                .clickable(onClick = onDisconnect),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = "断开连接",
                color = SW.red,
                style = MaterialTheme.typography.labelLarge,
            )
        }
        Spacer(Modifier.height(16.dp))
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(
        text = text,
        modifier = Modifier.padding(start = 4.dp, top = 18.dp, bottom = 8.dp),
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

@Composable
private fun SwitchRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    onChange: (Boolean) -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 52.dp)
            .padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(text = title, style = MaterialTheme.typography.bodyLarge, fontSize = 16.sp)
            if (subtitle.isNotEmpty()) {
                Text(
                    text = subtitle,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    fontSize = 13.sp,
                )
            }
        }
        Switch(
            checked = checked,
            onCheckedChange = onChange,
            colors = SwitchDefaults.colors(checkedTrackColor = SW.accent),
        )
    }
}

@Composable
private fun KeyboardPanel(controller: ControlController, onClose: () -> Unit) {
    var value by remember { mutableStateOf(TextFieldValue("")) }
    var sentText by remember { mutableStateOf("") }

    fun handleChange(next: TextFieldValue) {
        if (next.composition != null) {
            // 输入法还在组词，等上屏后再发。
            value = next
            return
        }
        val text = next.text
        val common = commonPrefixLength(text, sentText)
        val deletes = sentText.length - common
        if (deletes > 0) controller.sendBackspaces(deletes)
        if (text.length > common) controller.sendText(text.substring(common))
        sentText = text
        value = next
    }

    Surface(
        shape = RoundedCornerShape(22.dp),
        color = Color(0xF01C1C1E),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier
                        .weight(1f)
                        .clip(RoundedCornerShape(12.dp))
                        .background(Color.White.copy(alpha = 0.12f))
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                ) {
                    if (value.text.isEmpty()) {
                        Text(
                            text = "在这里输入，实时发送到远端…",
                            color = Color.White.copy(alpha = 0.45f),
                            fontSize = 15.sp,
                        )
                    }
                    BasicTextField(
                        value = value,
                        onValueChange = { handleChange(it) },
                        textStyle = TextStyle(color = Color.White, fontSize = 16.sp),
                        cursorBrush = androidx.compose.ui.graphics.SolidColor(SW.accent),
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
                Spacer(Modifier.width(8.dp))
                Text(
                    text = "收起",
                    color = SW.accent,
                    fontSize = 15.sp,
                    modifier = Modifier
                        .clip(RoundedCornerShape(10.dp))
                        .clickable(onClick = onClose)
                        .padding(horizontal = 10.dp, vertical = 8.dp),
                )
            }
            Spacer(Modifier.height(10.dp))
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                KeyChip("Esc") { controller.sendKey(0x01) }
                KeyChip("Tab") { controller.sendKey(0x0f) }
                KeyChip("↵") { controller.sendEnter() }
                KeyChip("←") { controller.sendKey(0x4b, ext = true) }
                KeyChip("↑") { controller.sendKey(0x48, ext = true) }
                KeyChip("↓") { controller.sendKey(0x50, ext = true) }
                KeyChip("→") { controller.sendKey(0x4d, ext = true) }
                KeyChip("Win") { controller.sendKey(0x5b, ext = true) }
                KeyChip("Ctrl+Alt+Del") { controller.sendSecureAttention() }
            }
        }
    }
}

@Composable
private fun KeyChip(label: String, onClick: () -> Unit) {
    Box(
        modifier = Modifier
            .heightIn(min = 40.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(Color.White.copy(alpha = 0.12f))
            .clickable(onClick = onClick)
            .padding(horizontal = 14.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(text = label, color = Color.White, fontSize = 14.sp)
    }
}

private fun statusText(stats: SessionStats): String {
    val fps = stats.fps.roundToInt()
    return if (stats.rttMs > 0) "$fps fps · ${stats.rttMs}ms" else "$fps fps"
}

private fun detailText(stats: SessionStats): String {
    val parts = mutableListOf<String>()
    if (stats.codec.isNotEmpty()) parts.add(stats.codec)
    if (stats.width > 0) parts.add("${stats.width}×${stats.height}")
    return parts.joinToString(" · ")
}

private fun commonPrefixLength(a: String, b: String): Int {
    var index = 0
    val limit = minOf(a.length, b.length)
    while (index < limit && a[index] == b[index]) index += 1
    return index
}
