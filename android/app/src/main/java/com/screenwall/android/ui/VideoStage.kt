package com.screenwall.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.foundation.clickable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.ui.unit.sp
import com.screenwall.android.session.ControlController
import com.screenwall.android.session.InputMode
import com.screenwall.android.session.Phase
import com.screenwall.android.video.H264SurfaceView
import com.screenwall.android.video.MjpegView
import kotlinx.coroutines.withTimeoutOrNull
import kotlin.math.abs
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * 画面 + 触摸手势：单指点击/拖动、长按右键、双指滚动、捏合缩放（1–4 倍）。
 */
@Composable
fun VideoStage(controller: ControlController, modifier: Modifier = Modifier) {
    val dims by controller.videoDims.collectAsState()
    val h264 by controller.isH264.collectAsState()
    val mode by controller.inputMode.collectAsState()

    var zoom by remember { mutableFloatStateOf(1f) }
    var pan by remember { mutableStateOf(Offset.Zero) }
    var boxSize by remember { mutableStateOf(IntSize.Zero) }
    val density = LocalDensity.current

    val rect: Rect? = remember(dims, boxSize) {
        val size = dims
        if (size == null || size.first <= 0 || size.second <= 0 ||
            boxSize.width <= 0 || boxSize.height <= 0
        ) {
            null
        } else {
            val scale = min(
                boxSize.width.toFloat() / size.first,
                boxSize.height.toFloat() / size.second,
            )
            val width = size.first * scale
            val height = size.second * scale
            val left = (boxSize.width - width) / 2f
            val top = (boxSize.height - height) / 2f
            Rect(left, top, left + width, top + height)
        }
    }
    val rectState = rememberUpdatedState(rect)
    val modeState = rememberUpdatedState(mode)
    val dimsState = rememberUpdatedState(dims)

    Box(
        modifier = modifier
            .fillMaxSize()
            .background(Color.Black)
            .onSizeChanged { boxSize = it },
    ) {
        if (rect != null) {
            val rectWidthDp = with(density) { rect.width.toDp() }
            val rectHeightDp = with(density) { rect.height.toDp() }
            Box(
                modifier = Modifier
                    .offset { IntOffset(rect.left.roundToInt(), rect.top.roundToInt()) }
                    .size(rectWidthDp, rectHeightDp)
                    .graphicsLayer(
                        scaleX = zoom,
                        scaleY = zoom,
                        translationX = pan.x,
                        translationY = pan.y,
                    ),
            ) {
                if (!h264) {
                    AndroidView(
                        factory = { context ->
                            MjpegView(context, controller.mjpeg) { width, height ->
                                controller.onMjpegFrame(width, height)
                            }
                        },
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    AndroidView(
                        factory = { context ->
                            H264SurfaceView(
                                context,
                                onSurface = { surface, width, height ->
                                    controller.markH264Surface(surface, width, height)
                                },
                                currentSize = { dimsState.value },
                            )
                        },
                        modifier = Modifier.fillMaxSize(),
                    )
                }
            }
        }

        // 手势层：铺满整个舞台，留白区域不发送坐标。
        Box(
            modifier = Modifier
                .fillMaxSize()
                .pointerInput(Unit) {
                    val touchSlop = viewConfiguration.touchSlop
                    awaitEachGesture {
                        if (controller.phase.value != Phase.Streaming) {
                            return@awaitEachGesture
                        }
                        val down = awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Main)
                        val downPosition = down.position
                        var lastPosition = downPosition
                        var moved = false
                        var twoFinger = false
                        var longPressed = false
                        var leftDown = false
                        var scrollAcc = 0f
                        var pinchStartDistance = 1f
                        var pinchStartZoom = 1f
                        var lastCentroidY = 0f
                        var pinching = false
                        val startedAt = System.currentTimeMillis()
                        val trackpad = modeState.value == InputMode.Trackpad

                        fun normalize(position: Offset): Pair<Float, Float>? {
                            val area = rectState.value ?: return null
                            if (area.width <= 0f || area.height <= 0f) return null
                            val centerX = area.center.x
                            val centerY = area.center.y
                            val currentZoom = zoom
                            val localX = centerX + (position.x - centerX - pan.x) / currentZoom
                            val localY = centerY + (position.y - centerY - pan.y) / currentZoom
                            val nx = (localX - area.left) / area.width
                            val ny = (localY - area.top) / area.height
                            if (nx < -0.02f || nx > 1.02f || ny < -0.02f || ny > 1.02f) return null
                            return nx.coerceIn(0f, 1f) to ny.coerceIn(0f, 1f)
                        }

                        val inside = normalize(downPosition) != null
                        if (inside && !trackpad) {
                            val point = normalize(downPosition)!!
                            controller.inputMoveAbs(point.first, point.second)
                            controller.inputButton("left", true, point.first, point.second)
                            leftDown = true
                            controller.flushInput()
                        }

                        while (true) {
                            val elapsed = System.currentTimeMillis() - startedAt
                            val waitingLongPress = inside && !moved && !twoFinger && !longPressed &&
                                elapsed < LONG_PRESS_MS
                            val event = if (waitingLongPress) {
                                withTimeoutOrNull(LONG_PRESS_MS - elapsed) { awaitPointerEvent() }
                            } else {
                                awaitPointerEvent()
                            }

                            if (event == null) {
                                // 长按 = 右键。
                                longPressed = true
                                if (leftDown) {
                                    controller.inputButton("left", false)
                                    leftDown = false
                                }
                                val point = normalize(downPosition)
                                if (point != null) {
                                    controller.inputButton("right", true, point.first, point.second)
                                    controller.inputButton("right", false, point.first, point.second)
                                }
                                controller.flushInput()
                                continue
                            }

                            val changes = event.changes
                            if (!inside) {
                                changes.forEach { it.consume() }
                                if (changes.all { !it.pressed }) break
                                continue
                            }
                            val pressedCount = changes.count { it.pressed }
                            if (pressedCount >= 2 && !twoFinger) {
                                twoFinger = true
                                if (leftDown) {
                                    controller.inputButton("left", false)
                                    leftDown = false
                                    controller.flushInput()
                                }
                                val pressed = changes.filter { it.pressed }
                                pinchStartDistance =
                                    (pressed[1].position - pressed[0].position).getDistance()
                                        .coerceAtLeast(1f)
                                pinchStartZoom = zoom
                                lastCentroidY = (pressed[0].position.y + pressed[1].position.y) / 2f
                                scrollAcc = 0f
                                pinching = false
                                changes.forEach { it.consume() }
                                continue
                            }
                            if (twoFinger) {
                                val pressed = changes.filter { it.pressed }
                                if (pressed.size >= 2) {
                                    val distance =
                                        (pressed[1].position - pressed[0].position).getDistance()
                                            .coerceAtLeast(1f)
                                    val ratio = distance / pinchStartDistance
                                    if (abs(ratio - 1f) > 0.06f) pinching = true
                                    val target = (pinchStartZoom * ratio).coerceIn(1f, 4f)
                                    val centroidY =
                                        (pressed[0].position.y + pressed[1].position.y) / 2f
                                    val dy = centroidY - lastCentroidY
                                    lastCentroidY = centroidY
                                    if (abs(target - zoom) > 0.002f) {
                                        zoom = target
                                        pan = if (zoom <= 1.001f) {
                                            Offset.Zero
                                        } else {
                                            clampPan(pan, rectState.value, zoom)
                                        }
                                    }
                                    if (!pinching && abs(dy) > 0.4f) {
                                        scrollAcc += dy
                                        while (abs(scrollAcc) >= 30f) {
                                            val direction = if (scrollAcc > 0) 1 else -1
                                            scrollAcc -= direction * 30f
                                            controller.inputWheel(0, -direction * 120)
                                        }
                                    }
                                }
                                changes.forEach { it.consume() }
                                if (changes.all { !it.pressed }) break
                                continue
                            }

                            val change = changes.firstOrNull { it.id == down.id } ?: changes.first()
                            if (!change.pressed) {
                                if (!moved && !longPressed) {
                                    if (trackpad) {
                                        controller.inputButton("left", true)
                                        controller.inputButton("left", false)
                                        controller.flushInput()
                                    } else if (leftDown) {
                                        val point = normalize(change.position)
                                        controller.inputButton(
                                            "left",
                                            false,
                                            point?.first,
                                            point?.second,
                                        )
                                        controller.flushInput()
                                    }
                                } else if (leftDown) {
                                    val point = normalize(change.position)
                                    controller.inputButton(
                                        "left",
                                        false,
                                        point?.first,
                                        point?.second,
                                    )
                                    controller.flushInput()
                                }
                                leftDown = false
                                change.consume()
                                break
                            }

                            val delta = change.position - lastPosition
                            lastPosition = change.position
                            if (!moved && (change.position - downPosition).getDistance() > touchSlop) {
                                moved = true
                            }
                            if (moved) {
                                if (zoom > 1.001f) {
                                    pan = clampPan(pan + delta, rectState.value, zoom)
                                } else if (trackpad) {
                                    if (abs(delta.x) >= 1f || abs(delta.y) >= 1f) {
                                        controller.inputMoveRel(
                                            delta.x.roundToInt(),
                                            delta.y.roundToInt(),
                                        )
                                    }
                                } else if (leftDown) {
                                    val point = normalize(change.position)
                                    if (point != null) {
                                        controller.inputMoveAbs(point.first, point.second)
                                    }
                                }
                            }
                            change.consume()
                        }
                        if (leftDown) {
                            controller.inputButton("left", false)
                            controller.flushInput()
                        }
                    }
                },
        )

        if (zoom > 1.01f) {
            Box(
                modifier = Modifier
                    .align(Alignment.CenterEnd)
                    .padding(end = 14.dp)
                    .clip(RoundedCornerShape(50))
                    .background(Color.Black.copy(alpha = 0.5f))
                    .clickable {
                        zoom = 1f
                        pan = Offset.Zero
                    }
                    .padding(horizontal = 12.dp, vertical = 8.dp),
            ) {
                Text(
                    text = "×${String.format(java.util.Locale.US, "%.1f", zoom)}",
                    color = Color.White,
                    fontSize = 12.sp,
                )
            }
        }
    }
}

private const val LONG_PRESS_MS = 550L

private fun clampPan(current: Offset, area: Rect?, zoom: Float): Offset {
    if (area == null) return Offset.Zero
    val maxX = area.width * (zoom - 1f) / 2f
    val maxY = area.height * (zoom - 1f) / 2f
    return Offset(
        current.x.coerceIn(-maxX, maxX),
        current.y.coerceIn(-maxY, maxY),
    )
}
