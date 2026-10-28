package net.talkee.talkee_app

import org.junit.Assert.assertEquals
import org.junit.Test

class OpusPacketTests {

    private fun toc(config: Int, code: Int) = byteArrayOf(((config shl 3) or (0 shl 2) or code).toByte(), 0, 0)

    @Test
    fun `a celt packet of 20 ms is 960 samples`() {
        // Config 19 is CELT fullband at 20 ms, the one a voice encoder at 48 kHz mostly produces.
        assertEquals(960, OpusEncoder.samplesIn(toc(19, 0)))
    }

    @Test
    fun `a silk packet of 20 ms is 960 samples`() {
        assertEquals(960, OpusEncoder.samplesIn(toc(1, 0)))
    }

    @Test
    fun `ten and sixty milliseconds`() {
        assertEquals(480, OpusEncoder.samplesIn(toc(0, 0)))
        assertEquals(2880, OpusEncoder.samplesIn(toc(3, 0)))
        assertEquals(480, OpusEncoder.samplesIn(toc(18, 0)))
    }

    @Test
    fun `two frames in a packet double it`() {
        assertEquals(1920, OpusEncoder.samplesIn(toc(1, 1)))
        assertEquals(1920, OpusEncoder.samplesIn(toc(1, 2)))
    }

    @Test
    fun `an arbitrary number of frames comes from the second byte`() {
        val packet = byteArrayOf(((19 shl 3) or 3).toByte(), 3, 0)
        assertEquals(3 * 960, OpusEncoder.samplesIn(packet))
    }

    @Test
    fun `packets that cannot be valid hold nothing`() {
        assertEquals(0, OpusEncoder.samplesIn(byteArrayOf()))
        assertEquals(0, OpusEncoder.samplesIn(byteArrayOf(((19 shl 3) or 3).toByte())))
        assertEquals(0, OpusEncoder.samplesIn(byteArrayOf(((19 shl 3) or 3).toByte(), 0)))
        // Sixty-three frames of 20 ms is far past the 120 ms limit.
        assertEquals(0, OpusEncoder.samplesIn(byteArrayOf(((19 shl 3) or 3).toByte(), 63)))
    }
}
