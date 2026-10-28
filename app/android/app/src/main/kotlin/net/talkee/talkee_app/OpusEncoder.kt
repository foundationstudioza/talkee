package net.talkee.talkee_app

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Compresses the microphone to Opus with the phone's own codec.
 *
 * Raw samples are 1.5 Mbit/s. Opus carries the same voice in about 40 times less, which on a crowded
 * Wi-Fi or a hotspot is the difference between steady audio and a stutter, and it lets the PC conceal
 * a lost packet instead of leaving a hole. Android ships an Opus encoder from Android 10; on an older
 * phone, or one without it, [available] is false and the app keeps sending raw samples.
 *
 * Fed 10 ms of audio at a time and handing back whole packets as the codec finishes them, which is
 * usually every second call: the codec works in 20 ms frames.
 */
class OpusEncoder private constructor(private val codec: MediaCodec) {

    /** One encoded packet and how many samples (at 48 kHz) of audio it holds. */
    class Packet(val bytes: ByteArray, val samples: Int)

    private val info = MediaCodec.BufferInfo()
    private var fed = 0L

    /**
     * Gives the codec [count] samples and returns every packet it has finished since the last call.
     *
     * Returns nothing rather than throwing when the codec is busy: one dropped 10 ms is a click, a
     * thrown exception in the capture loop is no microphone.
     */
    fun encode(pcm: FloatArray, count: Int): List<Packet> {
        val index = codec.dequeueInputBuffer(INPUT_WAIT_US)
        if (index >= 0) {
            val buffer = codec.getInputBuffer(index)
            if (buffer != null) {
                buffer.clear()
                buffer.order(ByteOrder.LITTLE_ENDIAN)
                for (i in 0 until count) {
                    // Float to the codec's 16-bit input, clipped rather than wrapped: a wrapped loud
                    // sample is a crack, a clipped one is merely loud.
                    val sample = (pcm[i] * 32767f).toInt().coerceIn(-32768, 32767)
                    buffer.putShort(sample.toShort())
                }
                val timeUs = fed * 1_000_000L / SAMPLE_RATE
                codec.queueInputBuffer(index, 0, count * 2, timeUs, 0)
                fed += count
            } else {
                codec.queueInputBuffer(index, 0, 0, 0, 0)
            }
        }
        return drain()
    }

    private fun drain(): List<Packet> {
        var packets: MutableList<Packet>? = null
        while (true) {
            val out = codec.dequeueOutputBuffer(info, 0)
            if (out < 0) {
                // INFO_TRY_AGAIN_LATER ends the loop; format and buffer-change notices carry nothing
                // this needs.
                if (out == MediaCodec.INFO_TRY_AGAIN_LATER) break
                continue
            }
            val buffer: ByteBuffer? = codec.getOutputBuffer(out)
            // The first buffer is the stream header (OpusHead), which the PC's decoder does not want.
            val isConfig = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
            if (buffer != null && info.size > 0 && !isConfig) {
                val bytes = ByteArray(info.size)
                buffer.position(info.offset)
                buffer.get(bytes)
                val samples = samplesIn(bytes)
                if (samples > 0) {
                    (packets ?: mutableListOf<Packet>().also { packets = it }).add(Packet(bytes, samples))
                }
            }
            codec.releaseOutputBuffer(out, false)
        }
        return packets ?: emptyList()
    }

    /** Throws away audio the codec is holding, for after a pause: it belongs to before the gap. */
    fun reset() {
        runCatching {
            codec.flush()
            codec.start()
        }
    }

    fun close() {
        runCatching { codec.stop() }
        runCatching { codec.release() }
    }

    companion object {
        private const val SAMPLE_RATE = 48_000
        private const val INPUT_WAIT_US = 10_000L

        /** Whether this phone has an Opus encoder to use. */
        fun available(): Boolean {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return false
            return runCatching {
                MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { codec ->
                    codec.isEncoder && codec.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_AUDIO_OPUS, true) }
                }
            }.getOrDefault(false)
        }

        /** An encoder at [bitrate] bits a second, or null if the codec would not start. */
        fun create(bitrate: Int): OpusEncoder? {
            if (!available()) return null
            var codec: MediaCodec? = null
            return try {
                val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, 1)
                format.setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
                format.setInteger(MediaFormat.KEY_PCM_ENCODING, android.media.AudioFormat.ENCODING_PCM_16BIT)
                codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
                codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
                codec.start()
                OpusEncoder(codec)
            } catch (_: Exception) {
                runCatching { codec?.release() }
                null
            }
        }

        /**
         * How many samples at 48 kHz an Opus packet holds, read from its first byte (RFC 6716 §3.1).
         *
         * The PC checks the header's sample count against the payload, so it has to be exactly what
         * the packet says; asking the codec is not an option, and the table is short. 0 for a packet
         * that cannot be valid.
         */
        fun samplesIn(packet: ByteArray): Int {
            if (packet.isEmpty()) return 0
            val toc = packet[0].toInt() and 0xFF
            val config = toc shr 3
            val perFrame = when {
                // SILK only: 10, 20, 40 or 60 ms.
                config < 12 -> intArrayOf(480, 960, 1920, 2880)[config % 4]
                // Hybrid: 10 or 20 ms.
                config < 16 -> intArrayOf(480, 960)[config % 2]
                // CELT only: 2.5, 5, 10 or 20 ms.
                else -> intArrayOf(120, 240, 480, 960)[config % 4]
            }
            val frames = when (toc and 0x03) {
                0 -> 1
                1, 2 -> 2
                else -> if (packet.size < 2) return 0 else packet[1].toInt() and 0x3F
            }
            val total = perFrame * frames
            // Opus never holds more than 120 ms.
            return if (frames == 0 || total > 5_760) 0 else total
        }
    }
}
