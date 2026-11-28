package com.screenwall.android.video

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Canvas
import android.graphics.RectF

/**
 * MJPEG 解码：后台线程只解最新一帧，旧的直接丢。
 *
 * 两块位图轮换：界面只读 `front`，解码器只写另一块，避免边画边被覆盖。
 */
class MjpegDecoder {
    private val lock = Object()
    private var front: Bitmap? = null
    private var back: Bitmap? = null
    private var pending: ByteArray? = null
    private var thread: Thread? = null

    @Volatile
    private var running = false

    /** 每解出一帧回调一次（在解码线程上），用于 `invalidate`。 */
    @Volatile
    var onDecoded: ((width: Int, height: Int) -> Unit)? = null

    private var frames = 0
    private var bytes = 0L
    private var lastSample = System.nanoTime()

    fun start() {
        synchronized(lock) {
            if (running) return
            running = true
        }
        thread = Thread({ loop() }, "screenwall-mjpeg").apply {
            isDaemon = true
            start()
        }
    }

    fun push(data: ByteArray) {
        synchronized(lock) {
            pending = data
            lock.notifyAll()
        }
        if (!running) start()
    }

    fun clear() {
        synchronized(lock) {
            pending = null
            front = null
            back = null
        }
    }

    fun stop() {
        val worker = thread
        synchronized(lock) {
            running = false
            pending = null
            lock.notifyAll()
        }
        worker?.join(500)
        thread = null
        clear()
    }

    fun draw(canvas: Canvas) {
        // 锁住整段绘制：解码线程切换缓冲时才不会把正在画的那块拿去写。
        synchronized(lock) {
            val bitmap = front ?: return
            if (bitmap.isRecycled) return
            canvas.drawBitmap(
                bitmap,
                null,
                RectF(0f, 0f, canvas.width.toFloat(), canvas.height.toFloat()),
                null,
            )
        }
    }

    fun frameSize(): Pair<Int, Int>? = synchronized(lock) {
        front?.let { if (it.isRecycled) null else it.width to it.height }
    }

    /** 复制当前帧（可缩小），用于保存最近连接缩略图；没有画面时返回 null。 */
    fun snapshot(maxWidth: Int = 480): Bitmap? = synchronized(lock) {
        val bitmap = front ?: return null
        if (bitmap.isRecycled) return null
        val copied = runCatching { bitmap.copy(Bitmap.Config.RGB_565, false) }.getOrNull() ?: return null
        if (copied.width <= maxWidth) return copied
        val scaled = runCatching {
            Bitmap.createScaledBitmap(
                copied,
                maxWidth,
                (copied.height.toLong() * maxWidth / copied.width).toInt().coerceAtLeast(1),
                true,
            )
        }.getOrNull()
        if (scaled == null || scaled === copied) {
            copied
        } else {
            copied.recycle()
            scaled
        }
    }

    /** 每秒调用一次：这期间的解码帧率与入站码率。 */
    fun sample(): Pair<Double, Int> {
        val now = System.nanoTime()
        val seconds = ((now - lastSample) / 1_000_000_000.0).coerceAtLeast(0.001)
        val fps = frames / seconds
        val kbps = (bytes * 8.0 / seconds / 1000.0).toInt()
        frames = 0
        bytes = 0
        lastSample = now
        return fps to kbps
    }

    private fun loop() {
        while (true) {
            val data = synchronized(lock) {
                while (running && pending == null) {
                    try {
                        lock.wait(200)
                    } catch (_: InterruptedException) {
                        return
                    }
                }
                if (!running) return
                pending.also { pending = null }
            } ?: continue

            val reuse = synchronized(lock) { back }
            val bitmap = decode(data, reuse)
            if (bitmap == null) {
                synchronized(lock) {
                    // JPEG 解到一半损坏时，inBitmap 内容不可信，直接丢掉避免下次复用。
                    back = null
                }
                continue
            }
            synchronized(lock) {
                val previousFront = front
                front = bitmap
                // 上一帧画完就空出来了，成为下一帧的复用缓冲。
                back = if (previousFront != null && previousFront !== bitmap && !previousFront.isRecycled) {
                    previousFront
                } else {
                    null
                }
                frames += 1
                bytes += data.size
            }
            onDecoded?.invoke(bitmap.width, bitmap.height)
        }
    }

    private fun decode(data: ByteArray, reuse: Bitmap?): Bitmap? {
        val options = BitmapFactory.Options().apply {
            inPreferredConfig = Bitmap.Config.RGB_565
            if (reuse != null && !reuse.isRecycled) {
                inMutable = true
                inBitmap = reuse
            }
        }
        val bitmap = try {
            BitmapFactory.decodeByteArray(data, 0, data.size, options)
        } catch (_: Throwable) {
            null
        }
        if (bitmap != null) return bitmap
        // 尺寸变了或复用失败：不带 inBitmap 再试一次。
        return try {
            BitmapFactory.decodeByteArray(
                data,
                0,
                data.size,
                BitmapFactory.Options().apply { inPreferredConfig = Bitmap.Config.RGB_565 },
            )
        } catch (_: Throwable) {
            null
        }
    }
}
