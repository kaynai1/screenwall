package com.screenwall.android.video

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.view.View

/** 画最新一帧 MJPEG 位图的自绘 View。 */
class MjpegView(
    context: Context,
    private val decoder: MjpegDecoder,
    private val onFrame: (width: Int, height: Int) -> Unit,
) : View(context) {
    init {
        setBackgroundColor(Color.BLACK)
        decoder.onDecoded = { width, height ->
            onFrame(width, height)
            postInvalidateOnAnimation()
        }
    }

    override fun onDraw(canvas: Canvas) {
        super.onDraw(canvas)
        decoder.draw(canvas)
    }

    override fun onDetachedFromWindow() {
        decoder.onDecoded = null
        super.onDetachedFromWindow()
    }
}
