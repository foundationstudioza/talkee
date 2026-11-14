package net.talkee.talkee_app

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.net.ServerSocket
import java.net.Socket
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Assume
import org.junit.Test

/**
 * The mesh dialer against the vectors `rust/talkee-mesh` produces, a fake listener, and (when
 * `TALKEE_MESH_LIVE_LINK` holds a fresh `talkeed pair` link) a real one.
 *
 * The vectors were printed by a throwaway Rust program that links `talkee-crypto`: HMAC with the
 * three labels, then `Session::new(link_key, SessionId [0;4], dir).seal(seq, aad = seq LE, ..)`.
 * Key = 0x00..0x1F, dialer nonce = 0x10..0x2F, listener nonce = 0x20..0x3F.
 */
class MeshTests {

    private val key = ByteArray(32) { it.toByte() }
    private val nd = ByteArray(32) { (0x10 + it).toByte() }
    private val nl = ByteArray(32) { (0x20 + it).toByte() }

    @Test
    fun fingerprint_and_proofs_match_rust() {
        assertEquals("630dcd29", Mesh.fingerprint(key))
        assertEquals(
            "1001439ad3162892c04fc29cde6f6f1d7e507c38bb9da9c734798fa75211efd1",
            Mesh.listenerProof(key, nd, nl).toHex(),
        )
        assertEquals(
            "3dec4de66c755711b7f6a99ddde6f6548fff15850feae82927943b98f6009d82",
            Mesh.dialerProof(key, nd, nl).toHex(),
        )
        assertEquals(
            "2ef52a50fe4c351bb94c588ecff3bfa247b07b3911c2483026266b1e30c70402",
            Mesh.linkKey(key, nd, nl).toHex(),
        )
    }

    @Test
    fun sealed_frames_match_rust_byte_for_byte() {
        val frameKey = Mesh.frameKey(Mesh.linkKey(key, nd, nl))
        // dialer -> listener, seq 0 and 1
        assertEquals(
            "0000000000000000000000001df2413e3906b01bcf7ab8c2a9518cf71ee60368fc383d74a2e965188ace4ecbb8f2bd9e",
            Crypto.seal(frameKey, 0, 0, """{"id":1,"op":"ping"}""".toByteArray(), byteArrayOf(0, 0, 0, 0)).toHex(),
        )
        assertEquals(
            "0001000000000000000000004e2706827f1bb50a4eb32c04cd77ebba270fb9a62d39cb1d8beb92669d85a0925ce991714349a52d63d2d2",
            Crypto.seal(frameKey, 0, 1, """{"id":2,"op":"device.info"}""".toByteArray(), byteArrayOf(1, 0, 0, 0)).toHex(),
        )
        // listener -> dialer, seq 0: opens here
        val back = "0100000000000000000000008ae89e3037643ce6228d5a2d91edb5b3c1fcd580566e507070a2345c6a93378fe305"
            .hexToBytes()!!
        val plain = Crypto.open(frameKey, back, byteArrayOf(0, 0, 0, 0))
        assertEquals("""{"id":1,"ok":true}""", String(plain!!))
    }

    /** The handshake with fixed nonces writes exactly the documented lines. */
    @Test
    fun the_handshake_writes_hello_then_the_dialer_proof() {
        val challenge = JSONObject().put("t", "MESH_CHALLENGE").put("nonce", nl.toHex())
            .put("proof", Mesh.listenerProof(key, nd, nl).toHex())
        val out = ByteArrayOutputStream()
        val frameKey = Mesh.handshake(
            ByteArrayInputStream((challenge.toString() + "\n").toByteArray()),
            out,
            key,
        ) { nd }
        val lines = out.toString("UTF-8").trim().split("\n").map { JSONObject(it) }
        assertEquals("MESH_HELLO", lines[0].getString("t"))
        assertEquals(1, lines[0].getInt("v"))
        assertEquals("630dcd29", lines[0].getString("fp"))
        assertEquals(nd.toHex(), lines[0].getString("nonce"))
        assertEquals("MESH_PROOF", lines[1].getString("t"))
        assertEquals(Mesh.dialerProof(key, nd, nl).toHex(), lines[1].getString("proof"))
        assertArrayEquals(Mesh.frameKey(Mesh.linkKey(key, nd, nl)), frameKey)
    }

    @Test
    fun a_listener_that_cannot_prove_the_key_is_refused() {
        val wrong = ByteArray(32) { 9 }
        val challenge = JSONObject().put("t", "MESH_CHALLENGE").put("nonce", nl.toHex())
            .put("proof", Mesh.listenerProof(wrong, nd, nl).toHex())
        try {
            Mesh.handshake(
                ByteArrayInputStream((challenge.toString() + "\n").toByteArray()),
                ByteArrayOutputStream(),
                key,
            ) { nd }
            fail("a bad proof must not complete the handshake")
        } catch (e: Mesh.MeshException) {
            assertTrue(e.message!!.contains("does not hold"))
        }
    }

    @Test
    fun a_deny_is_reported_in_the_listeners_words() {
        val deny = """{"t":"MESH_DENY","reason":"not paired with this device (or the invite expired)"}""" + "\n"
        try {
            Mesh.handshake(ByteArrayInputStream(deny.toByteArray()), ByteArrayOutputStream(), key) { nd }
            fail("deny must throw")
        } catch (e: Mesh.MeshException) {
            assertTrue(e.message!!.contains("invite expired"))
        }
    }

    // ---- a fake listener over a real socket ----

    /** The listener half of the protocol, enough for the phone's requests. */
    private class FakeListener(private val key: ByteArray, private val handler: (JSONObject) -> JSONObject) {
        val server = ServerSocket(0, 1, java.net.InetAddress.getByName("127.0.0.1"))
        val port get() = server.localPort
        val seen = mutableListOf<JSONObject>()
        @Volatile var failure: Throwable? = null

        private val thread = Thread {
            try {
                server.accept().use { serve(it) }
            } catch (t: Throwable) {
                failure = t
            }
        }.also { it.isDaemon = true; it.start() }

        private fun line(input: java.io.InputStream): JSONObject {
            val sb = StringBuilder()
            while (true) {
                val b = input.read()
                if (b == '\n'.code) break
                sb.append(b.toChar())
            }
            return JSONObject(sb.toString())
        }

        private fun serve(s: Socket) {
            val input = s.getInputStream()
            val out = s.getOutputStream()
            val hello = line(input)
            assertEquals(Mesh.fingerprint(key), hello.getString("fp"))
            val theirs = hello.getString("nonce").hexToBytes()!!
            val ours = ByteArray(32) { (0x40 + it).toByte() }
            out.write(
                (JSONObject().put("t", "MESH_CHALLENGE").put("nonce", ours.toHex())
                    .put("proof", Mesh.listenerProof(key, theirs, ours).toHex()).toString() + "\n").toByteArray(),
            )
            val proof = line(input)
            assertEquals(Mesh.dialerProof(key, theirs, ours).toHex(), proof.getString("proof"))
            val frameKey = Mesh.frameKey(Mesh.linkKey(key, theirs, ours))
            val din = DataInputStream(input)
            var expect = 0
            var send = 0
            while (true) {
                val first = input.read()
                if (first < 0) return
                val len = (first shl 24) or (din.readUnsignedByte() shl 16) or
                    (din.readUnsignedByte() shl 8) or din.readUnsignedByte()
                val body = ByteArray(len).also { din.readFully(it) }
                val aad = body.copyOfRange(0, 4)
                assertEquals(expect, aad[0].toInt() and 0xFF)
                val plain = Crypto.open(frameKey, body.copyOfRange(4, len), aad)!!
                // Direction bit 0 on the way in.
                assertEquals(0, body[4].toInt())
                expect++
                val req = JSONObject(String(plain))
                seen += req
                val resp = handler(req).put("id", req.getLong("id")).toString().toByteArray()
                val sealed = Crypto.seal(frameKey, 1, send, resp, byteArrayOf(send.toByte(), 0, 0, 0))
                val l = 4 + sealed.size
                out.write(byteArrayOf((l ushr 24).toByte(), (l ushr 16).toByte(), (l ushr 8).toByte(), l.toByte()))
                out.write(byteArrayOf(send.toByte(), 0, 0, 0))
                out.write(sealed)
                out.flush()
                send++
            }
        }
    }

    @Test
    fun joining_sends_this_phone_and_keeps_the_new_key() {
        val newKey = ByteArray(32) { (0xA0 + it).toByte() }
        val listener = FakeListener(key) { req ->
            assertEquals("pair.complete", req.getString("op"))
            JSONObject().put("ok", true).put(
                "body",
                JSONObject()
                    .put("device", JSONObject().put("id", "ab".repeat(16)).put("name", "compaq").put("kind", "server")
                        .put("caps", org.json.JSONArray(listOf("agents"))))
                    .put("key", java.util.Base64.getUrlEncoder().withoutPadding().encodeToString(newKey)),
            )
        }
        val link = PairLink("127.0.0.1", listener.port, "compaq", Mesh.fingerprint(key), key)
        val peer = MeshClient.join(link, "cd".repeat(16), "Pixel Test")
        assertNull(listener.failure)

        val sent = listener.seen.single().getJSONObject("body")
        assertEquals("cd".repeat(16), sent.getString("id"))
        assertEquals("Pixel Test", sent.getString("name"))
        assertEquals("phone", sent.getString("kind"))
        assertEquals("[\"mic\",\"dictate\"]", sent.getJSONArray("caps").toString())

        assertEquals("ab".repeat(16), peer.id)
        assertEquals("compaq", peer.name)
        assertEquals("server", peer.kind)
        assertArrayEquals("the answer's new key is kept, not the invite's", newKey, peer.key)
        assertEquals("127.0.0.1:${listener.port}", peer.addr)
        assertFalse("the public view carries no key", peer.toPublicJson().toString().contains("key"))
    }

    @Test
    fun requests_and_answers_keep_sequence_over_several_calls() {
        val listener = FakeListener(key) { JSONObject().put("ok", true).put("body", JSONObject().put("n", 1)) }
        Mesh.dial("127.0.0.1", listener.port, key).use { ch ->
            repeat(3) { assertEquals(1, ch.call("ping").getJSONObject("body").getInt("n")) }
        }
        assertEquals(3, listener.seen.size)
        assertEquals(3L, listener.seen.last().getLong("id"))
    }

    @Test
    fun an_error_answer_surfaces_the_listeners_message() {
        val listener = FakeListener(key) { JSONObject().put("ok", false).put("error", "unknown operation x") }
        Mesh.dial("127.0.0.1", listener.port, key).use { ch ->
            try {
                ch.call("x")
                fail()
            } catch (e: Mesh.MeshException) {
                assertEquals("unknown operation x", e.message)
            }
        }
    }

    @Test
    fun stored_peers_round_trip_and_upsert_replaces() {
        val a = MeshPeer("a".repeat(32), "one", "server", listOf("agents"), key, "10.0.0.2:47810")
        val b = MeshPeer("b".repeat(32), "two", "pc", emptyList(), key, "[::1]:47810")
        val text = MeshPeers.encode(listOf(a, b))
        val back = MeshPeers.decode(text)
        assertEquals(listOf("one", "two"), back.map { it.name })
        assertArrayEquals(key, back[0].key)
        val newer = MeshPeer(a.id, "one!", "server", emptyList(), ByteArray(32) { 1 }, a.addr)
        val merged = MeshPeers.upsert(back, newer)
        assertEquals(2, merged.size)
        assertEquals("one!", merged.first { it.id == a.id }.name)
        assertEquals("10.0.0.2", MeshPeers.host(a.addr))
        assertEquals("::1", MeshPeers.host(b.addr))
        assertEquals(47810, MeshPeers.port(b.addr))
        assertTrue(MeshPeers.decode("not json").isEmpty())
    }

    /** Against a real listener: `TALKEE_MESH_LIVE_LINK='talkee://pair?...p=47810...' ./gradlew ...`. */
    @Test
    fun live_join_then_device_info() {
        val text = System.getenv("TALKEE_MESH_LIVE_LINK")
        Assume.assumeTrue("set TALKEE_MESH_LIVE_LINK to a fresh `talkeed pair` link", !text.isNullOrBlank())
        val link = PairLink.parse(text!!)
        assertNotNull(link)
        val peer = MeshClient.join(link!!, "ef".repeat(16), "Unit test phone")
        val info = JSONObject(MeshClient.call(peer, "device.info"))
        assertEquals(peer.id, info.getString("id"))
        assertEquals(peer.kind, info.getString("kind"))
        MeshClient.call(peer, "ping")
        try {
            MeshClient.call(peer, "pair.complete")
            fail("a used invite must not complete again")
        } catch (e: Mesh.MeshException) {
            // expected
        }
    }

    private fun assertNull(t: Throwable?) {
        if (t != null) throw AssertionError("fake listener failed", t)
    }
}
