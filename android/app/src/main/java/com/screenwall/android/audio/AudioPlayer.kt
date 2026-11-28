package com.screenwall.android.audio

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import com.screenwall.android.net.Protocol
import java.nio.ByteBuffer

/**
 * 远端声音播放：Opus 走 MediaCodec 解码，PCM16 直接写 AudioTrack。
 *
 * 入站帧在 WebSocket 线程上 push，实际解码/播放放到单独线程，避免堵住视频。
 */
class AudioPlayer(
    private val onError: (String) -> Unit,
) {
    private val lock = Object()
    private val queue = ArrayDeque<ByteArray>()
    private var running = false
    private var thread: Thread? = null
    private var sampleRate = 48_000
    private var channels = 2
    private var useOpus = false
    private var track: AudioTrack? = null
    private var codec: MediaCodec? = null

    /** 配置播放器；返回 false 表示这台机器解不了该编码（Opus）。 */
    fun configure(codecName: String, sampleRate: Int, channels: Int): Boolean {
        close()
        synchronized(lock) {
            this.sampleRate = if (sampleRate > 0) sampleRate else 48_000
            this.channels = channels.coerceIn(1, 2)
            this.useOpus = codecName == "opus"
            queue.clear()
            running = true
        }
        if (useOpus) {
            val created = try {
                MediaCodec.createDecoderByType(OPUS_MIME)
            } catch (_: Throwable) {
                null
            } ?: run {
                synchronized(lock) { running = false }
                return false
            }
            val format = MediaFormat.createAudioFormat(
                OPUS_MIME,
                this.sampleRate,
                this.channels,
            ).apply {
                setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 8192)
            }
            try {
                created.configure(format, null, null, 0)
                created.start()
            } catch (_: Throwable) {
                runCatching { created.release() }
                synchronized(lock) { running = false }
                return false
            }
            codec = created
        }
        val trackCreated = createTrack(this.sampleRate, this.channels) ?: run {
            codec?.let { runCatching { it.release() } }
            codec = null
            synchronized(lock) { running = false }
            return false
        }
        track = trackCreated
        thread = Thread({ loop() }, "screenwall-audio").apply {
            isDaemon = true
            start()
        }
        return true
    }

    fun push(packetCodec: Int, data: ByteArray) {
        synchronized(lock) {
            if (!running) return
            if (queue.size > 48) {
                // 播放跟不上就丢最老的，宁可断一下也别越积越延迟。
                queue.removeFirst()
            }
            queue.addLast(data)
            lock.notifyAll()
        }
    }

    fun close() {
        val worker = thread
        synchronized(lock) {
            running = false
            queue.clear()
            lock.notifyAll()
        }
        worker?.join(500)
        thread = null
        codec?.let { decoder ->
            runCatching { decoder.stop() }
            runCatching { decoder.release() }
        }
        codec = null
        track?.let { audioTrack ->
            runCatching { audioTrack.pause() }
            runCatching { audioTrack.flush() }
            runCatching { audioTrack.release() }
        }
        track = null
    }

    private fun loop() {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val packet = synchronized(lock) {
                while (running && queue.isEmpty()) {
                    try {
                        lock.wait(200)
                    } catch (_: InterruptedException) {
                        return
                    }
                }
                if (!running) return
                queue.removeFirst()
            }
            try {
                if (useOpus) {
                    playOpus(packet, info)
                } else {
                    playPcm(packet)
                }
            } catch (error: Throwable) {
                onError("声音播放失败：${error.message ?: error.javaClass.simpleName}")
                return
            }
        }
    }

    private fun playPcm(data: ByteArray) {
        val audioTrack = track ?: return
        audioTrack.write(data, 0, data.size)
    }

    private fun playOpus(packet: ByteArray, info: MediaCodec.BufferInfo) {
        val decoder = codec ?: return
        val inputIndex = decoder.dequeueInputBuffer(10_000)
        if (inputIndex >= 0) {
            val input = decoder.getInputBuffer(inputIndex)
            if (input != null) {
                input.clear()
                input.put(packet)
                decoder.queueInputBuffer(inputIndex, 0, packet.size, 0, 0)
            } else {
                decoder.queueInputBuffer(inputIndex, 0, 0, 0, 0)
            }
        }
        while (true) {
            val outputIndex = decoder.dequeueOutputBuffer(info, 0)
            if (outputIndex >= 0) {
                val output: ByteBuffer? = decoder.getOutputBuffer(outputIndex)
                if (output != null && info.size > 0) {
                    val bytes = ByteArray(info.size)
                    output.position(info.offset)
                    output.get(bytes, 0, info.size)
                    track?.write(bytes, 0, bytes.size)
                }
                decoder.releaseOutputBuffer(outputIndex, false)
            } else if (outputIndex == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                // 部分解码器输出格式会变，按输出采样率重建 AudioTrack。
                val format = decoder.outputFormat
                val rate = format.getInteger(MediaFormat.KEY_SAMPLE_RATE, sampleRate)
                val count = format.getInteger(MediaFormat.KEY_CHANNEL_COUNT, channels)
                if (rate != sampleRate || count != channels) {
                    sampleRate = rate
                    channels = count.coerceIn(1, 2)
                    track?.let {
                        runCatching { it.pause() }
                        runCatching { it.flush() }
                        runCatching { it.release() }
                    }
                    track = createTrack(sampleRate, channels)
                }
            } else {
                break
            }
        }
    }

    private fun createTrack(sampleRate: Int, channels: Int): AudioTrack? {
        val channelMask = if (channels == 1) {
            AudioFormat.CHANNEL_OUT_MONO
        } else {
            AudioFormat.CHANNEL_OUT_STEREO
        }
        val minBuffer = AudioTrack.getMinBufferSize(
            sampleRate,
            channelMask,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        if (minBuffer <= 0) return null
        val bufferSize = minBuffer * 2
        return try {
            AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                        .build(),
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(sampleRate)
                        .setChannelMask(channelMask)
                        .build(),
                )
                .setBufferSizeInBytes(bufferSize)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()
                .also { it.play() }
        } catch (_: Throwable) {
            null
        }
    }

    companion object {
        private const val OPUS_MIME = "audio/opus"
    }
}
