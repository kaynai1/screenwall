package com.screenwall.android.net

import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * 中继隧道帧与媒体帧的编解码。
 *
 * 与 `crates/proto/src/cloud.rs`（8 字节会话帧）和
 * `crates/proto/src/frame.rs`（24 字节媒体帧）逐字节对应，改动任何一边都要同步。
 */
object Protocol {
    const val TUNNEL_HEADER_LEN = 8
    const val MEDIA_HEADER_LEN = 24

    const val TUNNEL_JSON = 1
    const val TUNNEL_BINARY = 2

    const val MEDIA_VIDEO = 1
    const val MEDIA_AUDIO = 2
    const val MEDIA_FILE = 3

    const val CODEC_JPEG = 1
    const val CODEC_H264 = 2
    const val CODEC_PCM16 = 3
    const val CODEC_OPUS = 4

    const val FLAG_KEYFRAME = 0x01

    /** 组 8 字节会话帧头 + 负载。 */
    fun encodeTunnel(kind: Int, session: Int, payload: ByteArray): ByteArray {
        val out = ByteArray(TUNNEL_HEADER_LEN + payload.size)
        out[0] = 1
        out[1] = kind.toByte()
        ByteBuffer.wrap(out, 2, 4).order(ByteOrder.LITTLE_ENDIAN).putInt(session)
        out[6] = 0
        out[7] = 0
        System.arraycopy(payload, 0, out, TUNNEL_HEADER_LEN, payload.size)
        return out
    }

    fun decodeTunnel(data: ByteArray): TunnelFrame? {
        if (data.size < TUNNEL_HEADER_LEN || (data[0].toInt() and 0xFF) != 1) return null
        val kind = data[1].toInt() and 0xFF
        if (kind != TUNNEL_JSON && kind != TUNNEL_BINARY) return null
        val session = ByteBuffer.wrap(data, 2, 4).order(ByteOrder.LITTLE_ENDIAN).int
        return TunnelFrame(kind, session, data.copyOfRange(TUNNEL_HEADER_LEN, data.size))
    }

    fun decodeMediaFrame(data: ByteArray): MediaFrame? {
        if (data.size < MEDIA_HEADER_LEN || (data[0].toInt() and 0xFF) != 1) return null
        val buffer = ByteBuffer.wrap(data).order(ByteOrder.LITTLE_ENDIAN)
        val payloadLen = buffer.getInt(16)
        if (payloadLen != data.size - MEDIA_HEADER_LEN) return null
        return MediaFrame(
            type = data[1].toInt() and 0xFF,
            streamId = buffer.getShort(2).toInt() and 0xFFFF,
            seq = buffer.getInt(4),
            tsUs = buffer.getLong(8),
            codec = data[20].toInt() and 0xFF,
            flags = data[21].toInt() and 0xFF,
            payload = data.copyOfRange(MEDIA_HEADER_LEN, data.size),
        )
    }

    fun tunnelJson(session: Int, message: String): ByteArray =
        encodeTunnel(TUNNEL_JSON, session, message.toByteArray(Charsets.UTF_8))
}

data class TunnelFrame(val kind: Int, val session: Int, val payload: ByteArray)

data class MediaFrame(
    val type: Int,
    val streamId: Int,
    val seq: Int,
    val tsUs: Long,
    val codec: Int,
    val flags: Int,
    val payload: ByteArray,
) {
    val isKeyframe: Boolean get() = (flags and Protocol.FLAG_KEYFRAME) != 0
}
