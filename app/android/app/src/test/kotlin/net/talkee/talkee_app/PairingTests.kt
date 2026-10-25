package net.talkee.talkee_app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.fail
import org.junit.Assume
import org.junit.Test

/**
 * The link parser, and the handshake against a real host.
 *
 * The live tests are skipped unless `TALKEE_LIVE_LINK` holds a `talkee://pair` link from a running
 * `talkee host --secure`, so this suite is green on a machine with nothing to pair with — the same
 * arrangement as `talkee-discovery`'s ignored live-network test.
 */
class PairingTests {

    /** A link of the shape the desktop actually builds. */
    private val link =
        "talkee://pair?v=1&h=192.168.0.24&p=47800&fpid=298fbbd2" +
            "&name=Talkee%20on%20DESKTOP-EXAMPLE&k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU"

    @Test
    fun a_link_says_where_to_connect_and_what_to_prove() {
        val parsed = PairLink.parse(link)
        assertNotNull("the real link must parse", parsed)
        assertEquals("192.168.0.24", parsed!!.host)
        assertEquals(47800, parsed.controlPort)
        assertEquals(
            "the audio port is the control port plus one, as the protocol says",
            47801,
            parsed.audioPort,
        )
        assertEquals("Talkee on DESKTOP-EXAMPLE", parsed.name)
        assertEquals("298fbbd2", parsed.fingerprint)
        assertEquals(32, parsed.key.size)
    }

    @Test
    fun a_scan_that_is_not_a_talkee_link_is_refused_rather_than_thrown() {
        // The camera sees whatever is in front of it. Every one of these has to come back as
        // "not a Talkee code" instead of a crash on launch.
        for (text in listOf(
            "",
            "not a uri at all",
            "https://discord.gg/invite",
            "talkee://pair",
            "talkee://pair?",
            // A newer link than this build understands.
            link.replace("v=1", "v=2"),
            // No host, no port, no key.
            "talkee://pair?v=1&p=47800&k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU",
            "talkee://pair?v=1&h=192.168.0.24&k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU",
            "talkee://pair?v=1&h=192.168.0.24&p=47800",
            // A port that is not a port.
            link.replace("p=47800", "p=99999"),
            // A key that is not 32 bytes once decoded.
            link.replace("k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU", "k=AAAA"),
            "talkee://pair?v=1&h=h&p=1&k='not base64!'",
        )) {
            assertNull("should have been refused: $text", PairLink.parse(text))
        }
    }

    @Test
    fun the_name_is_percent_decoded_while_the_key_is_left_alone() {
        // The name is escaped in the link and has to come back readable. The key must *not* go
        // through a URL decoder: it is base64url, which contains neither `%` nor `+`, so decoding it
        // could only corrupt it — and a corrupted key is a pairing that fails for no visible reason.
        val parsed = PairLink.parse(link)!!
        assertEquals("Talkee on DESKTOP-EXAMPLE", parsed.name)

        // The published vector's key, to prove the bytes are exactly what base64url said.
        val vector = PairLink.parse(
            "talkee://pair?v=1&h=h&p=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        )!!
        assertEquals(
            (0 until 32).map { it.toByte() },
            vector.key.toList(),
        )
    }

    @Test
    fun hex_round_trips() {
        val bytes = ByteArray(32) { it.toByte() }
        assertEquals(bytes.toList(), bytes.toHex().hexToBytes()!!.toList())
        assertEquals("00ff10", byteArrayOf(0, 0xFF.toByte(), 0x10).toHex())
        // Odd length and non-hex are "not useful", not faults.
        assertNull("abc".hexToBytes())
        assertNull("zz".hexToBytes())
    }

    /** The link from the environment, or skip: with no host running there is nothing to pair with. */
    private fun liveLink(): PairLink {
        val text = System.getenv("TALKEE_LIVE_LINK")
        Assume.assumeTrue(
            "set TALKEE_LIVE_LINK to a `talkee://pair` link from a running host to run this",
            !text.isNullOrBlank(),
        )
        val parsed = PairLink.parse(text!!)
        assertNotNull("TALKEE_LIVE_LINK is not a Talkee pairing link", parsed)
        return parsed!!
    }

    @Test
    fun a_real_host_accepts_a_proof_this_code_computes() {
        // The assertion no unit test can make: the host verifies the proof with its *Rust*
        // implementation, so this is the two languages agreeing about a protocol in public.
        val paired = Control.pair(liveLink(), "jvm-test-device", "JVM test")
        try {
            assertEquals("the host asks for 48 kHz", 48_000, paired.config.sampleRate)
            assertEquals("and one channel", 1, paired.config.channels)
        } finally {
            // Closing this is what drops the session on the host, so it is not optional.
            paired.connection.close()
        }
    }

    @Test
    fun a_real_host_refuses_a_proof_from_the_wrong_key() {
        // Without this the test above would pass even if the host accepted anything at all. One
        // flipped byte in the key must be refused, and refused with a reason rather than a timeout.
        val good = liveLink()
        val wrong = good.copy(key = good.key.copyOf().also { it[0] = (it[0].toInt() xor 0xFF).toByte() })
        try {
            Control.pair(wrong, "jvm-test-device", "JVM test")
            fail("the host accepted a proof computed from the wrong key")
        } catch (expected: Control.Rejected) {
            assertNotNull("a refusal should carry a reason", expected.message)
        }
    }
}
