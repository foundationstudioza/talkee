package net.talkee.talkee_app

import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.io.DataInputStream
import java.io.InputStream
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64
import org.json.JSONArray
import org.json.JSONObject

/**
 * The phone's side of the device mesh (TCP 47810), as a dialer.
 *
 * Specified in `docs/PROTOCOL.md` ("Device mesh") and implemented on the other side by
 * `rust/talkee-mesh`. Like [Crypto] this has no Android framework dependency, so a plain JVM test can
 * run it against a fake listener and against pinned vectors computed by the Rust crate.
 *
 * A link is: a clear-text handshake in which both sides prove the pair key under different labels
 * (so a proof cannot be reflected), then length-prefixed AES-256-GCM frames under a per-connection
 * link key. Nothing here logs: a key or a link never reaches a message, an exception text or a file.
 */
object Mesh {

    /** The port a mesh invite (and a mesh listener) uses. The microphone link is 47800. */
    const val PORT = 47810

    const val VERSION = 1

    /** Largest frame either side accepts, length field included. */
    const val MAX_FRAME = 1 shl 20

    private const val MAX_LINE = 1024
    private const val NONCE_LEN = 32

    private val LISTENER_LABEL = "talkee-mesh-listener-v1".toByteArray(Charsets.US_ASCII)
    private val DIALER_LABEL = "talkee-mesh-dialer-v1".toByteArray(Charsets.US_ASCII)
    private val LINK_LABEL = "talkee-mesh-link-v1".toByteArray(Charsets.US_ASCII)

    /** The session id the mesh always uses; the link key is already unique per connection. */
    private val SESSION_ID = ByteArray(Crypto.SESSION_ID_LEN)

    /** First 4 bytes of SHA-256 of the key, in hex: how a listener finds which key is meant. */
    fun fingerprint(key: ByteArray): String =
        MessageDigest.getInstance("SHA-256").digest(key).copyOf(4).toHex()

    /** HMAC-SHA256(key, "talkee-mesh-listener-v1" || Nd || Nl). */
    fun listenerProof(key: ByteArray, dialerNonce: ByteArray, listenerNonce: ByteArray): ByteArray =
        Crypto.hmacSha256(key, LISTENER_LABEL, dialerNonce, listenerNonce)

    /** HMAC-SHA256(key, "talkee-mesh-dialer-v1" || Nd || Nl). */
    fun dialerProof(key: ByteArray, dialerNonce: ByteArray, listenerNonce: ByteArray): ByteArray =
        Crypto.hmacSha256(key, DIALER_LABEL, dialerNonce, listenerNonce)

    /** HMAC-SHA256(key, "talkee-mesh-link-v1" || Nd || Nl): new for every connection. */
    fun linkKey(key: ByteArray, dialerNonce: ByteArray, listenerNonce: ByteArray): ByteArray =
        Crypto.hmacSha256(key, LINK_LABEL, dialerNonce, listenerNonce)

    /** The AES key frames are sealed with: `Session::new(link_key, SessionId [0;4], ..)` in Rust. */
    fun frameKey(linkKey: ByteArray): ByteArray = Crypto.sessionKey(linkKey, SESSION_ID)

    /** Constant-time comparison of two proofs. */
    private fun sameProof(a: ByteArray, b: ByteArray): Boolean = MessageDigest.isEqual(a, b)

    class MeshException(message: String) : Exception(message)

    /**
     * Dials `host:port`, proves `key`, and returns the encrypted channel.
     *
     * `random` is injectable only so a test can fix the nonce; production uses [SecureRandom].
     */
    fun dial(
        host: String,
        port: Int,
        key: ByteArray,
        connectTimeoutMs: Int = 5000,
        readTimeoutMs: Int = 30000,
        random: (Int) -> ByteArray = { n -> ByteArray(n).also { SecureRandom().nextBytes(it) } },
    ): Channel {
        val socket = Socket()
        try {
            socket.tcpNoDelay = true
            socket.connect(InetSocketAddress(host, port), connectTimeoutMs)
            socket.soTimeout = 10000
            val frameKey = handshake(socket.getInputStream(), socket.getOutputStream(), key, random)
            socket.soTimeout = readTimeoutMs
            return Channel(socket, socket.getInputStream(), socket.getOutputStream(), frameKey)
        } catch (e: Exception) {
            runCatching { socket.close() }
            throw e
        }
    }

    /** Runs the handshake over `input`/`output` and returns the frame key. */
    internal fun handshake(
        input: InputStream,
        output: OutputStream,
        key: ByteArray,
        random: (Int) -> ByteArray,
    ): ByteArray {
        val ours = random(NONCE_LEN)
        val hello = JSONObject()
            .put("t", "MESH_HELLO")
            .put("v", VERSION)
            .put("fp", fingerprint(key))
            .put("nonce", ours.toHex())
        output.write((hello.toString() + "\n").toByteArray(Charsets.UTF_8))
        output.flush()

        val reply = JSONObject(readLine(input))
        when (reply.optString("t")) {
            "MESH_CHALLENGE" -> {}
            "MESH_DENY" -> throw MeshException("refused: ${reply.optString("reason", "no reason given")}")
            else -> throw MeshException("unexpected handshake message")
        }
        val theirs = reply.optString("nonce").hexToBytes()
        val claimed = reply.optString("proof").hexToBytes()
        if (theirs == null || theirs.size != NONCE_LEN || claimed == null) {
            throw MeshException("the other device sent a malformed challenge")
        }
        if (!sameProof(claimed, listenerProof(key, ours, theirs))) {
            throw MeshException("the other device does not hold this pairing's key")
        }
        val proof = JSONObject()
            .put("t", "MESH_PROOF")
            .put("proof", dialerProof(key, ours, theirs).toHex())
        output.write((proof.toString() + "\n").toByteArray(Charsets.UTF_8))
        output.flush()
        return frameKey(linkKey(key, ours, theirs))
    }

    /** One handshake line, read byte by byte so no frame byte is consumed with it. */
    private fun readLine(input: InputStream): String {
        val buf = ByteArrayOutputStream()
        while (true) {
            val b = input.read()
            if (b < 0) throw MeshException("the other device closed the connection")
            if (b == '\n'.code) break
            if (buf.size() >= MAX_LINE) throw MeshException("handshake line too long")
            buf.write(b)
        }
        return buf.toString("UTF-8")
    }

    /**
     * An open mesh link: sealed frames in both directions.
     *
     * Not thread-safe; one request at a time, which is all the phone needs.
     */
    class Channel internal constructor(
        private val closeable: Closeable?,
        input: InputStream,
        private val output: OutputStream,
        private val frameKey: ByteArray,
    ) : Closeable {
        private val input = DataInputStream(input)
        private var sendSeq = 0
        private var recvSeq = 0
        private var nextId = 1L

        /** Seals and writes one frame (dialer to listener). */
        fun send(plain: ByteArray) {
            val seq = sendSeq
            val aad = seqBytes(seq)
            val sealed = Crypto.seal(frameKey, DIALER_TO_LISTENER, seq, plain, aad)
            val len = 4 + sealed.size
            if (len > MAX_FRAME) throw MeshException("message too large for the mesh")
            sendSeq = seq + 1
            val frame = ByteArray(4 + len)
            frame[0] = (len ushr 24).toByte()
            frame[1] = (len ushr 16).toByte()
            frame[2] = (len ushr 8).toByte()
            frame[3] = len.toByte()
            System.arraycopy(aad, 0, frame, 4, 4)
            System.arraycopy(sealed, 0, frame, 8, sealed.size)
            output.write(frame)
            output.flush()
        }

        /** Reads and opens one frame, or null when the other side closed cleanly. */
        fun recv(): ByteArray? {
            val first = input.read()
            if (first < 0) return null
            val len = (first shl 24) or (input.readUnsignedByte() shl 16) or
                (input.readUnsignedByte() shl 8) or input.readUnsignedByte()
            if (len < 4 || len > MAX_FRAME) throw MeshException("frame of $len bytes refused")
            val body = ByteArray(len)
            input.readFully(body)
            val aad = body.copyOfRange(0, 4)
            val seq = (aad[0].toInt() and 0xFF) or ((aad[1].toInt() and 0xFF) shl 8) or
                ((aad[2].toInt() and 0xFF) shl 16) or ((aad[3].toInt() and 0xFF) shl 24)
            if (seq != recvSeq) throw MeshException("frame $seq out of order")
            val sealed = body.copyOfRange(4, len)
            if (sealed.size < Crypto.AEAD_OVERHEAD ||
                !sealed.copyOfRange(0, Crypto.NONCE_LEN).contentEquals(Crypto.nonce(LISTENER_TO_DIALER, seq))
            ) {
                throw MeshException("frame refused")
            }
            val plain = Crypto.open(frameKey, sealed, aad) ?: throw MeshException("frame refused")
            recvSeq = seq + 1
            return plain
        }

        /** One request, one answer. Throws with the other device's own words when it says no. */
        fun call(op: String, body: JSONObject? = null): JSONObject {
            val id = nextId++
            val req = JSONObject().put("id", id).put("op", op)
            if (body != null) req.put("body", body)
            send(req.toString().toByteArray(Charsets.UTF_8))
            val bytes = recv() ?: throw MeshException("the other device hung up")
            val resp = JSONObject(String(bytes, Charsets.UTF_8))
            if (resp.optLong("id", -1) != id) throw MeshException("answer for a different request")
            if (!resp.optBoolean("ok", false)) throw MeshException(resp.optString("error", "failed"))
            return JSONObject().put("body", resp.opt("body") ?: JSONObject.NULL)
        }

        override fun close() {
            runCatching { closeable?.close() }
        }
    }

    private fun seqBytes(seq: Int) = byteArrayOf(
        seq.toByte(), (seq ushr 8).toByte(), (seq ushr 16).toByte(), (seq ushr 24).toByte(),
    )

    private const val DIALER_TO_LISTENER = 0
    private const val LISTENER_TO_DIALER = 1
}

/** A device this phone has joined. `key` is the pair key and is never sent to Dart or logged. */
class MeshPeer(
    val id: String,
    val name: String,
    val kind: String,
    val caps: List<String>,
    val key: ByteArray,
    val addr: String,
) {
    /** What Dart may see: everything except the key. */
    fun toPublicJson(): JSONObject = JSONObject()
        .put("id", id).put("name", name).put("kind", kind)
        .put("caps", JSONArray(caps)).put("addr", addr)

    fun toStoredJson(): JSONObject = toPublicJson()
        .put("key", Base64.getUrlEncoder().withoutPadding().encodeToString(key))

    companion object {
        fun fromStoredJson(o: JSONObject): MeshPeer? {
            val key = try {
                Base64.getUrlDecoder().decode(o.optString("key"))
            } catch (_: IllegalArgumentException) {
                return null
            }
            if (key.size != Crypto.PSK_LEN) return null
            val id = o.optString("id").takeIf { it.isNotEmpty() } ?: return null
            val caps = o.optJSONArray("caps")?.let { a -> (0 until a.length()).map { a.optString(it) } }
                ?: emptyList()
            return MeshPeer(id, o.optString("name", id), o.optString("kind", "pc"), caps, key, o.optString("addr"))
        }
    }
}

/** The set of peers, as the JSON text that [MeshStore] keeps encrypted. */
object MeshPeers {
    fun encode(peers: List<MeshPeer>): String =
        JSONArray(peers.map { it.toStoredJson() }).toString()

    fun decode(text: String): List<MeshPeer> = try {
        val a = JSONArray(text)
        (0 until a.length()).mapNotNull { MeshPeer.fromStoredJson(a.getJSONObject(it)) }
    } catch (_: Exception) {
        emptyList()
    }

    /** Replaces any peer with the same id, so joining again does not leave a stale key behind. */
    fun upsert(peers: List<MeshPeer>, peer: MeshPeer): List<MeshPeer> =
        peers.filter { it.id != peer.id } + peer

    fun host(addr: String): String =
        if (addr.startsWith("[")) addr.substringBefore("]").removePrefix("[") else addr.substringBeforeLast(':')

    fun port(addr: String): Int = addr.substringAfterLast(':').toIntOrNull() ?: Mesh.PORT

    fun addr(host: String, port: Int): String = if (host.contains(':')) "[$host]:$port" else "$host:$port"
}

/**
 * Joining and calling. Everything blocks on the network, so callers run it off the main thread.
 */
object MeshClient {

    /** This phone as a mesh device, minus nothing secret. */
    fun deviceJson(id: String, name: String): JSONObject = JSONObject()
        .put("id", id)
        .put("name", name)
        .put("kind", "phone")
        .put("caps", JSONArray(listOf("mic", "dictate")))

    /**
     * Joins the mesh a scanned invite belongs to, returning the new peer (not yet stored).
     *
     * The key in the invite is single-use; the answer carries the pair's real key, which is what is
     * kept. Throws [Mesh.MeshException] or an I/O exception with a sentence worth showing.
     */
    fun join(link: PairLink, myId: String, myName: String): MeshPeer {
        val channel = Mesh.dial(link.host, link.controlPort, link.key)
        channel.use { ch ->
            val answer = ch.call("pair.complete", deviceJson(myId, myName)).getJSONObject("body")
            val them = answer.getJSONObject("device")
            val key = try {
                Base64.getUrlDecoder().decode(answer.getString("key"))
            } catch (_: Exception) {
                throw Mesh.MeshException("the other device sent no usable key")
            }
            if (key.size != Crypto.PSK_LEN) throw Mesh.MeshException("the other device sent no usable key")
            val id = them.getString("id")
            if (id.length != 32) throw Mesh.MeshException("the other device sent an unusable id")
            val caps = them.optJSONArray("caps")?.let { a -> (0 until a.length()).map { a.optString(it) } }
                ?: emptyList()
            return MeshPeer(
                id = id,
                name = them.optString("name", link.name).ifBlank { link.name },
                kind = them.optString("kind", "server"),
                caps = caps,
                key = key,
                addr = MeshPeers.addr(link.host, link.controlPort),
            )
        }
    }

    /** Calls one operation on a peer and returns its answer body as JSON text. */
    fun call(peer: MeshPeer, op: String): String {
        Mesh.dial(MeshPeers.host(peer.addr), MeshPeers.port(peer.addr), peer.key).use { ch ->
            return ch.call(op).opt("body")?.toString() ?: "null"
        }
    }
}
