package net.talkee.talkee_app

import javax.crypto.Cipher
import javax.crypto.Mac
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec

/**
 * The phone half of the wire crypto, byte for byte with `rust/talkee-crypto`.
 *
 * # Why this is not Rust
 *
 * The committed architecture puts shared logic in the Rust core, and this belongs there eventually.
 * For now it is here because the alternative is cross-compiling the crypto crate for four ABIs to
 * compute one HMAC per session — and because a second implementation in another language is
 * genuinely useful: if the two disagree, a test can say so, instead of a user discovering it as
 * silence.
 *
 * That is why `CryptoTests` pins both derivations to the vector `rust/talkee-crypto` publishes, which
 * was itself computed by .NET's `HMACSHA256`. Three implementations agreeing is a protocol; one
 * implementation agreeing with itself is a coincidence.
 *
 * # What is deliberately absent
 *
 * Nothing here knows about sockets, JSON, or the audio header. It computes a proof, derives a session
 * key, and seals or opens a byte array. Keeping it that small is what makes it testable on a plain
 * JVM with no device and no Android framework — and it leaves exactly one place where the labels and
 * the nonce layout live.
 */
object Crypto {

    /** Bytes in a pairing key, which is also the AES-256 key size. */
    const val PSK_LEN = 32

    /** Bytes in a challenge, which is what `CHALLENGE` carries in hex. */
    const val CHALLENGE_LEN = 32

    /** Bytes in a session id. */
    const val SESSION_ID_LEN = 4

    /** Bytes of GCM nonce prepended to every sealed payload. */
    const val NONCE_LEN = 12

    /** Bytes of GCM tag appended to every sealed payload. */
    const val TAG_LEN = 16

    /** Bytes a sealed payload carries on top of its plaintext. */
    const val AEAD_OVERHEAD = NONCE_LEN + TAG_LEN

    /** The direction bit for frames going from the phone to the desktop. */
    const val PHONE_TO_DESKTOP = 0

    /**
     * Domain separation for the two derivations.
     *
     * These labels *are* part of the protocol: changing either one invalidates every pairing, which
     * is why they sit next to the code that uses them rather than in a config file.
     */
    private val PAIR_LABEL = "talkee-pair-v1".toByteArray(Charsets.US_ASCII)
    private val SESSION_LABEL = "talkee-session-v1".toByteArray(Charsets.US_ASCII)

    /** HMAC-SHA256 over the concatenation of `parts`. */
    private fun hmacSha256(key: ByteArray, vararg parts: ByteArray): ByteArray {
        val mac = Mac.getInstance("HmacSHA256")
        mac.init(SecretKeySpec(key, "HmacSHA256"))
        for (part in parts) {
            mac.update(part)
        }
        return mac.doFinal()
    }

    /**
     * The `PAIR.proof` value, as the lowercase hex the wire carries.
     *
     * `nonce` is the raw [CHALLENGE_LEN] bytes of the challenge, not its hex text — the one place
     * where getting the encoding wrong produces a proof that is self-consistent and rejected.
     */
    fun proofHex(psk: ByteArray, nonce: ByteArray, sessionId: ByteArray): String =
        hmacSha256(psk, PAIR_LABEL, nonce, sessionId).toHex()

    /**
     * The key every frame in one session is sealed with.
     *
     * `HMAC-SHA256(psk, "talkee-session-v1" || session_id)` is HKDF-Extract with the pairing key as
     * the salt. There is no Expand step because one 32-byte output is all AES-256 needs.
     */
    fun sessionKey(psk: ByteArray, sessionId: ByteArray): ByteArray =
        hmacSha256(psk, SESSION_LABEL, sessionId)

    /**
     * The GCM nonce for one direction and sequence number.
     *
     * `[direction bit][seq as little-endian u32][7 zero bytes]`, which is only safe because `seq`
     * never repeats under one session key — see the crate docs on why a key is derived per stream
     * rather than the pairing key being used directly.
     */
    fun nonce(directionBit: Int, seq: Int): ByteArray {
        val nonce = ByteArray(NONCE_LEN)
        nonce[0] = directionBit.toByte()
        nonce[1] = (seq and 0xFF).toByte()
        nonce[2] = ((seq ushr 8) and 0xFF).toByte()
        nonce[3] = ((seq ushr 16) and 0xFF).toByte()
        nonce[4] = ((seq ushr 24) and 0xFF).toByte()
        return nonce
    }

    /**
     * Seals one frame into `[nonce][ciphertext][tag]`, which is what `ENCRYPTED` marks.
     *
     * `aad` is the 24-byte audio header. It is authenticated but not encrypted, and the host opens
     * with the same bytes: leaving it out here made every frame fail its tag on the PC, which showed
     * up as a phone that paired successfully and a microphone that stayed silent.
     */
    fun seal(
        key: ByteArray,
        directionBit: Int,
        seq: Int,
        plaintext: ByteArray,
        aad: ByteArray = ByteArray(0),
    ): ByteArray {
        val nonce = nonce(directionBit, seq)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(
            Cipher.ENCRYPT_MODE,
            SecretKeySpec(key, "AES"),
            GCMParameterSpec(TAG_LEN * 8, nonce),
        )
        cipher.updateAAD(aad)
        return nonce + cipher.doFinal(plaintext)
    }

    /**
     * Opens one sealed frame, returning `null` when it does not authenticate.
     *
     * `null` rather than an exception because a bad tag is an ordinary event on a network: a
     * datagram from somebody else, a replay, or a corrupted frame. Every one of those is a dropped
     * frame, and none of them is a crash.
     */
    fun open(key: ByteArray, payload: ByteArray, aad: ByteArray = ByteArray(0)): ByteArray? {
        if (payload.size < AEAD_OVERHEAD) {
            return null
        }
        val nonce = payload.copyOfRange(0, NONCE_LEN)
        val sealed = payload.copyOfRange(NONCE_LEN, payload.size)
        return try {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(
                Cipher.DECRYPT_MODE,
                SecretKeySpec(key, "AES"),
                GCMParameterSpec(TAG_LEN * 8, nonce),
            )
            cipher.updateAAD(aad)
            cipher.doFinal(sealed)
        } catch (_: Exception) {
            null
        }
    }
}

/** Lowercase hex, because the protocol's hex is lowercase on both sides. */
internal fun ByteArray.toHex(): String {
    val out = StringBuilder(size * 2)
    for (b in this) {
        val v = b.toInt() and 0xFF
        out.append(HEX_DIGITS[v ushr 4])
        out.append(HEX_DIGITS[v and 0x0F])
    }
    return out.toString()
}

/**
 * Decodes hex, returning `null` when the text is not hex.
 *
 * `null` rather than an exception: this is fed by the network and by a camera, and every caller so
 * far treats "not hex" as "not useful" rather than as a fault.
 */
internal fun String.hexToBytes(): ByteArray? {
    val text = trim()
    if (text.length % 2 != 0) {
        return null
    }
    val out = ByteArray(text.length / 2)
    for (i in out.indices) {
        val high = Character.digit(text[i * 2], 16)
        val low = Character.digit(text[i * 2 + 1], 16)
        if (high < 0 || low < 0) {
            return null
        }
        out[i] = ((high shl 4) or low).toByte()
    }
    return out
}

private const val HEX_DIGITS = "0123456789abcdef"
