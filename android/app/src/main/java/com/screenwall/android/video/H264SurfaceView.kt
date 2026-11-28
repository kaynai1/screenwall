package com.screenwall.android.video

import android.content.Context
import android.graphics.SurfaceTexture
import android.view.Surface
import android.view.TextureView

/** 给 MediaCodec 当输出面的 TextureView，负责创建/释放 Surface。 */
class H264SurfaceView(
    context: Context,
    private val onSurface: (Surface?, Int, Int) -> Unit,
    private val currentSize: () -> Pair<Int, Int>?,
) : TextureView(context) {
    private var surface: Surface? = null

    init {
        surfaceTextureListener = object : SurfaceTextureListener {
            override fun onSurfaceTextureAvailable(texture: SurfaceTexture, width: Int, height: Int) {
                val created = Surface(texture)
                surface = created
                val size = currentSize()
                onSurface(created, size?.first ?: 0, size?.second ?: 0)
            }

            override fun onSurfaceTextureSizeChanged(texture: SurfaceTexture, width: Int, height: Int) = Unit

            override fun onSurfaceTextureDestroyed(texture: SurfaceTexture): Boolean {
                onSurface(null, 0, 0)
                surface?.release()
                surface = null
                return true
            }

            override fun onSurfaceTextureUpdated(texture: SurfaceTexture) = Unit
        }
    }

    override fun onDetachedFromWindow() {
        onSurface(null, 0, 0)
        surface?.release()
        surface = null
        super.onDetachedFromWindow()
    }
}
