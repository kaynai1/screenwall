package com.screenwall.android.video

import android.media.MediaCodec
import android.media.MediaFormat
import android.view.Surface
import com.screenwall.android.net.MediaFrame
import com.screenwall.android.net.Protocol
import java.nio.ByteBuffer

/**
 * H.264 硬解：Annex-B 码流 → MediaCodec → Surface（TextureView）。
 *
 * 只在关键帧里取 SPS/PPS 作为 csd，等 Surface 就绪后才真正配置解码器；
 * 解不了时回调 [onError]，由上层退回 MJPEG。
 */
class H264Decoder(
    private val onFrameSize: (width: Int, height: Int) -> Unit,
    private val onError: () -> Unit,
) {
    private val lock = Object()
    private val queue = ArrayDeque<MediaFrame>()
    private var running = false
    private var thread: Thread? = null
    private var surface: Surface? = null
    private var expectedWidth = 0
    private var expectedHeight = 0
    private var surfaceVersion = 0
    private var appliedVersion = -1

    @Volatile
    private var broken = false

    private var codec: MediaCodec? = null
    private var frames = 0
    private var lastSample = System.nanoTime()

    fun start() {
        synchronized(lock) {
            if (running) return
            running = true
        }
        thread = Thread({ loop() }, "screenwall-h264").apply {
            isDaemon = true
            start()
        }
    }

    fun setSurface(value: Surface?, width: Int, height: Int) {
        synchronized(lock) {
            if (surface !== value) {
                surfaceVersion += 1
            }
            surface = value
            if (width > 0) expectedWidth = width
            if (height > 0) expectedHeight = height
            lock.notifyAll()
        }
    }

    fun setExpectedSize(width: Int, height: Int) {
        synchronized(lock) {
            if (width > 0 && height > 0) {
                if (width != expectedWidth || height != expectedHeight) {
                    expectedWidth = width
                    expectedHeight = height
                    surfaceVersion += 1
                }
            }
            lock.notifyAll()
        }
    }

    fun push(frame: MediaFrame) {
        if (broken) return
        synchronized(lock) {
            if (queue.size >= 6) {
                // 丢掉最旧的非关键帧，保持低延迟。
                val iterator = queue.iterator()
                while (iterator.hasNext()) {
                    if (!iterator.next().isKeyframe) {
                        iterator.remove()
                        break
                    }
                }
            }
            queue.addLast(frame)
            lock.notifyAll()
        }
    }

    /** 切换/清空订阅时调用：丢掉未解完的帧，下个关键帧重新配置。 */
    fun reset() {
        synchronized(lock) {
            queue.clear()
            surfaceVersion += 1
            broken = false
            lock.notifyAll()
        }
    }

    fun sample(): Double {
        val now = System.nanoTime()
        val seconds = ((now - lastSample) / 1_000_000_000.0).coerceAtLeast(0.001)
        val fps = frames / seconds
        frames = 0
        lastSample = now
        return fps
    }

    fun stop() {
        val worker = thread
        synchronized(lock) {
            running = false
            queue.clear()
            lock.notifyAll()
        }
        worker?.join(1000)
        thread = null
        codec = null
        synchronized(lock) {
            surface = null
        }
    }

    private fun loop() {
        try {
            while (true) {
                val frame = synchronized(lock) {
                    while (running && queue.isEmpty()) {
                        if (surfaceVersion != appliedVersion) break
                        try {
                            lock.wait(150)
                        } catch (_: InterruptedException) {
                            return
                        }
                    }
                    if (!running) return
                    if (surfaceVersion != appliedVersion) {
                        appliedVersion = surfaceVersion
                        queue.clear()
                        releaseCodec()
                        null
                    } else {
                        queue.removeFirstOrNull()
                    }
                } ?: continue

                if (broken) continue
                feed(frame)
                drain()
            }
        } finally {
            releaseCodec()
        }
    }

    private fun feed(frame: MediaFrame) {
        val decoder = codec ?: run {
            if (!frame.isKeyframe) return
            createCodec(frame) ?: return
        }
        val index = try {
            decoder.dequeueInputBuffer(if (frame.isKeyframe) 10_000 else 4_000)
        } catch (error: Throwable) {
            fail(error)
            return
        }
        if (index < 0) return
        val buffer = decoder.getInputBuffer(index)
        if (buffer == null) {
            decoder.queueInputBuffer(index, 0, 0, frame.tsUs, 0)
            return
        }
        buffer.clear()
        buffer.put(frame.payload)
        decoder.queueInputBuffer(
            index,
            0,
            frame.payload.size,
            frame.tsUs,
            if (frame.isKeyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0,
        )
    }

    private fun createCodec(frame: MediaFrame): MediaCodec? {
        val (target, width, height) = synchronized(lock) { Triple(surface, expectedWidth, expectedHeight) }
        if (target == null || width <= 0 || height <= 0) return null
        val csd = extractCsd(frame.payload) ?: return null
        return try {
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
            format.setByteBuffer("csd-0", ByteBuffer.wrap(csd.first))
            format.setByteBuffer("csd-1", ByteBuffer.wrap(csd.second))
            format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 4 * 1024 * 1024)
            val created = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            created.configure(format, target, null, 0)
            created.start()
            codec = created
            created
        } catch (error: Throwable) {
            fail(error)
            null
        }
    }

    private fun drain() {
        val decoder = codec ?: return
        val info = MediaCodec.BufferInfo()
        try {
            while (true) {
                val index = decoder.dequeueOutputBuffer(info, 0)
                when {
                    index >= 0 -> {
                        decoder.releaseOutputBuffer(index, true)
                        frames += 1
                    }
                    index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                        val format = decoder.outputFormat
                        val width = format.getInteger("width")
                        val height = format.getInteger("height")
                        onFrameSize(width, height)
                    }
                    index == MediaCodec.INFO_OUTPUT_BUFFERS_CHANGED -> Unit
                    else -> break
                }
            }
        } catch (error: Throwable) {
            fail(error)
        }
    }

    private fun fail(error: Throwable) {
        if (broken) return
        broken = true
        synchronized(lock) { queue.clear() }
        releaseCodec()
        onError()
    }

    private fun releaseCodec() {
        val current = codec ?: return
        codec = null
        runCatching { current.stop() }
        runCatching { current.release() }
    }

    companion object {
        /** 从 Annex-B 里取 SPS 与 PPS（各自带 4 字节起始码）。 */
        fun extractCsd(data: ByteArray): Pair<ByteArray, ByteArray>? {
            var sps: ByteArray? = null
            var pps: ByteArray? = null
            var index = 0
            while (index + 3 < data.size) {
                if (data[index] != 0.toByte() || data[index + 1] != 0.toByte() || data[index + 2] != 1.toByte()) {
                    index += 1
                    continue
                }
                val header = index + 3
                if (header >= data.size) break
                val type = data[header].toInt() and 0x1f
                var end = data.size
                var scan = header + 1
                while (scan + 2 < data.size) {
                    if (data[scan] == 0.toByte() && data[scan + 1] == 0.toByte() && data[scan + 2] == 1.toByte()) {
                        end = scan
                        break
                    }
                    scan += 1
                }
                var trimmed = end
                while (trimmed > header && data[trimmed - 1] == 0.toByte()) trimmed -= 1
                if (type == 7 && sps == null) {
                    sps = byteArrayOf(0, 0, 0, 1) + data.copyOfRange(header, trimmed)
                } else if (type == 8 && pps == null) {
                    pps = byteArrayOf(0, 0, 0, 1) + data.copyOfRange(header, trimmed)
                }
                index = end
            }
            val spsBytes = sps
            val ppsBytes = pps
            return if (spsBytes != null && ppsBytes != null) spsBytes to ppsBytes else null
        }
    }
}
