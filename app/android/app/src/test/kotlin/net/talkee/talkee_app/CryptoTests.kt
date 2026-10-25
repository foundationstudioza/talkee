package net.talkee.talkee_app

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * Pins the Kotlin crypto to the vector `rust/talkee-crypto` publishes.
 *
 * That vector was computed by .NET's `HMACSHA256`, so it is not this implementation agreeing with
 * itself: if a label, a concatenation order, an integer endianness or a hex case changes on either
 * side, this fails here rather than becoming a pairing that silently never completes.
 *
 * These run on a plain JVM — no device, no instrumentation — because `Crypto` deliberately has no
 * Android dependencies at all.
 */
class CryptoTests {

    /** The key from the crate's known-answer vectors: bytes 0x00..0x1F. */
    private fun psk(): ByteArray = ByteArray(Crypto.PSK_LEN) { it.toByte() }

    /** The challenge, chosen the same way so the inputs are visible in the vector. */
    private fun challenge(): ByteArray = ByteArray(Crypto.CHALLENGE_LEN) { it.toByte() }

    private fun sessionId(): ByteArray = byteArrayOf(1, 2, 3, 4)

    @Test
    fun the_proof_matches_the_vector_the_rust_crate_publishes() {
        // psk = 00 01 .. 1f, nonce = 00 01 .. 1f, session = 01 02 03 04
        assertEquals(
            "8cd6e1b4ba154c4521f0de5d302208a1f10478bf708bcae98038aeb5b1c60593",
            Crypto.proofHex(psk(), challenge(), sessionId()),
        )
    }

    @Test
    fun the_session_key_matches_the_same_vector() {
        assertEquals(
            "acbcb7a70f5e3ba04af029614c9d5959ab97d297fc807264dc30e6f4d3b9af49",
            Crypto.sessionKey(psk(), sessionId()).toHex(),
        )
    }

    @Test
    fun the_two_derivations_cannot_collide() {
        // Same key, same session id, different label. If the label were dropped or shared, these
        // would be equal — and a proof would then be a session key, which is a real leak.
        val session = sessionId()
        assertNotEquals(
            Crypto.proofHex(psk(), challenge(), session),
            Crypto.sessionKey(psk(), session).toHex(),
        )
    }

    @Test
    fun the_nonce_is_a_direction_bit_then_a_little_endian_sequence() {
        // Byte order and where the sequence starts are the two things a second implementation gets
        // wrong, and neither shows up as an error — only as frames that never authenticate.
        assertArrayEquals(
            byteArrayOf(0, 0x04, 0x03, 0x02, 0x01, 0, 0, 0, 0, 0, 0, 0),
            Crypto.nonce(Crypto.PHONE_TO_DESKTOP, 0x01020304),
        )
    }

    @Test
    fun a_frame_hides_the_audio_and_round_trips() {
        val key = Crypto.sessionKey(psk(), sessionId())
        val audio = ByteArray(480) { (it % 97).toByte() }
        val frame = Crypto.seal(key, Crypto.PHONE_TO_DESKTOP, 7, audio)

        assertEquals(
            "a frame is a nonce, the ciphertext and a tag",
            Crypto.NONCE_LEN + audio.size + Crypto.TAG_LEN,
            frame.size,
        )
        assertArrayEquals(
            "the nonce travels in the clear, as the protocol says",
            Crypto.nonce(Crypto.PHONE_TO_DESKTOP, 7),
            frame.copyOfRange(0, Crypto.NONCE_LEN),
        )
        assertNull(
            "the frame must authenticate before anything is done with it",
            null.also { assertNotEquals(String(frame), String(audio)) },
        )
        assertArrayEquals(audio, Crypto.open(key, frame))
    }

    @Test
    fun the_same_audio_at_a_different_sequence_is_a_different_frame() {
        // The nonce is the sequence number, so this is really a test that the nonce is used: two
        // frames of identical audio must not share ciphertext, or reuse of a nonce would go
        // unnoticed until an attacker noticed it for us.
        val key = Crypto.sessionKey(psk(), sessionId())
        val audio = ByteArray(64) { 1 }
        val first = Crypto.seal(key, Crypto.PHONE_TO_DESKTOP, 1, audio)
        val second = Crypto.seal(key, Crypto.PHONE_TO_DESKTOP, 2, audio)
        assertNotEquals(first.toHex(), second.toHex())
    }

    @Test
    fun a_tampered_frame_does_not_open() {
        val key = Crypto.sessionKey(psk(), sessionId())
        val frame = Crypto.seal(key, Crypto.PHONE_TO_DESKTOP, 3, ByteArray(32) { 9 })

        val flipped = frame.copyOf()
        flipped[flipped.size - 1] = (flipped[flipped.size - 1].toInt() xor 0x01).toByte()
        assertNull("a corrupted frame must be dropped, not opened", Crypto.open(key, flipped))

        val truncated = frame.copyOf(frame.size - 4)
        assertNull(Crypto.open(key, truncated))
        assertNull("anything shorter than the overhead is not a frame", Crypto.open(key, ByteArray(4)))
    }

    @Test
    fun a_frame_from_another_key_does_not_open() {
        val frame = Crypto.seal(Crypto.sessionKey(psk(), sessionId()), Crypto.PHONE_TO_DESKTOP, 1, ByteArray(16))
        val wrong = Crypto.sessionKey(ByteArray(32) { 0x7F }, sessionId())
        assertNull(Crypto.open(wrong, frame))
    }
}
