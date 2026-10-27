package net.talkee.talkee_app

import org.json.JSONArray
import org.json.JSONObject
import java.io.BufferedReader
import java.io.Closeable
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.io.Writer
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.charset.StandardCharsets
import java.security.SecureRandom

/**
 * The pairing handshake: `HELLO`, `CHALLENGE`, `PAIR`, `CONFIG`.
 *
 * # What it is for
 *
 * The host accepts no audio at all until this succeeds, which is the whole reason a microphone on a
 * shared network is defensible. The key is never sent: the host picks a nonce, and the phone answers
 * `HMAC-SHA256(psk, "talkee-pair-v1" || nonce || session_id)`. An eavesdropper records something
 * worthless against the next challenge.
 *
 * # The one structural thing here
 *
 * **The socket must stay open for the life of the stream.** The host installs the session when the
 * proof verifies and *drops* it when this connection closes, so a phone that pairs and then hangs up
 * is a phone whose audio is refused — and it would look like an audio bug rather than a control one.
 * That is why [pair] hands the connection back rather than keeping it, and why nothing here closes
 * it for you.
 *
 * # What it deliberately does not do
 *
 * No audio, no threading, no retries, no state. It connects, proves, receives the encoding, and
 * returns. Everything about *when* to do that belongs to the engine.
 */
object Control {

    /** The protocol version this build speaks. Must match `talkee_proto::PROTOCOL_VERSION`. */
    const val PROTOCOL_VERSION = 1

    /** How long any single step of the handshake may take. */
    private const val TIMEOUT_MS = 10_000

    /** Bytes of session id. Fresh for every connection, and covered by the proof. */
    private const val SESSION_ID_LEN = 4

    /** How often the phone pings the PC while paired. */
    private const val PING_EVERY_MS = 5_000L

    /** Silence from the PC for this long means it is gone. Three pings' worth, so one lost packet is not. */
    private const val DEAD_AFTER_MS = 15_000

    /** The encoding the host asked for. */
    data class Config(
        val sampleRate: Int,
        val channels: Int,
        val format: Int,
        val frameMs: Int,
        val source: String,
    )

    /**
     * A live pairing.
     *
     * [connection] must be kept, and closed only when streaming stops. Letting it go early silently
     * unpairs the phone.
     */
    class Paired(
        val sessionKey: ByteArray,
        val config: Config,
        val connection: Connection,
    )

    /**
     * The control connection itself.
     *
     * Returned rather than hidden, because its lifetime *is* the pairing's lifetime and a caller that
     * cannot see it cannot keep it.
     */
    class Connection internal constructor(
        private val socket: Socket,
        private val input: BufferedReader,
        private val out: Writer,
    ) : Closeable {
        val isOpen: Boolean get() = !socket.isClosed

        /**
         * True once the host has hung up or the network under it has gone.
         *
         * Audio is UDP, which says nothing when nobody is listening, so this connection is the only
         * way the phone ever learns that the PC restarted or went away. Without it a phone keeps
         * streaming into a host that no longer holds its session, and looks connected while the PC
         * refuses every frame.
         */
        @Volatile var lost: Boolean = false
            private set

        /**
         * Reads what the host says for as long as the connection lasts.
         *
         * Uses the *same* reader the handshake used. The host sends its first `STATE` immediately
         * after `CONFIG`, and the handshake's buffered reader may already hold it: a second reader on
         * the same socket would never see those bytes.
         *
         * [onState] is called on the watcher thread with the desktop's switches. The end of the
         * stream flips [lost].
         */
        internal fun watch(onState: (paused: Boolean, noise: Boolean, echo: Boolean) -> Unit) {
            // Tells the PC this phone is still there, and gets an answer that proves the PC is. Without
            // this a phone that dies (or a PC that sleeps) is only noticed minutes later, by whichever
            // side next tries to say something. The PC echoes every ping, so silence for longer than
            // [DEAD_AFTER_MS] means the other end is gone.
            Thread({
                try {
                    while (!lost && !socket.isClosed) {
                        Thread.sleep(PING_EVERY_MS)
                        send(JSONObject().put("type", "PING").put("ts", (System.nanoTime() / 1_000_000L).toInt() and 0x7fffffff))
                    }
                } catch (_: Exception) {
                    // The watcher below notices the same failure and flips [lost].
                }
            }, "talkee-control-ping").apply { isDaemon = true }.start()

            Thread({
                try {
                    socket.soTimeout = DEAD_AFTER_MS
                    while (true) {
                        val line = input.readLine() ?: break
                        val message = try { JSONObject(line) } catch (_: Exception) { continue }
                        if (message.optString("type") == "STATE") {
                            onState(
                                message.optBoolean("paused", false),
                                message.optBoolean("noise_suppression", true),
                                message.optBoolean("echo_cancel", true),
                            )
                        }
                    }
                } catch (_: Exception) {
                    // A reset is the same news as a clean close.
                }
                lost = true
            }, "talkee-control-watch").apply { isDaemon = true }.start()
        }

        /** Asks the host to change a setting. Blocks on the network: never call it from the UI thread. */
        @Synchronized
        fun send(message: JSONObject) {
            out.write(message.toString())
            out.write("\n")
            out.flush()
        }

        override fun close() {
            // Closing an already-closed socket is not worth propagating: this is called from a stop
            // path where the point is to end up closed, not to learn whether it already was.
            runCatching { socket.close() }
        }
    }

    /** Raised when the host refused the proof, carrying the reason it gave. */
    class Rejected(reason: String) : Exception(reason)

    /**
     * Connects, proves possession of the key, and returns what to seal frames with.
     *
     * Throws [Rejected] when the host says no — a wrong key, or a host that has rotated its own — and
     * lets a network failure surface as an `IOException`, because those are different problems with
     * different fixes and the caller should be able to say which happened.
     */
    fun pair(link: PairLink, deviceId: String, deviceName: String): Paired {
        val socket = Socket()
        try {
            socket.connect(InetSocketAddress(link.host, link.controlPort), TIMEOUT_MS)
            socket.soTimeout = TIMEOUT_MS
            val out = OutputStreamWriter(socket.getOutputStream(), StandardCharsets.UTF_8)
            val input = BufferedReader(InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8))

            // 1. HELLO.
            send(
                out,
                JSONObject()
                    .put("type", "HELLO")
                    .put("ver", PROTOCOL_VERSION)
                    .put("dev_id", deviceId)
                    .put("name", deviceName)
                    // A real JSON array, not a stringified list. `JSONObject.put` with a Kotlin `List`
                    // serialises it as the *string* "[aes-gcm, pcm-f32]" — which is not what the
                    // protocol says, and which the host rejects outright: `caps` deserialises into a
                    // Vec<String>, so a string there fails the whole HELLO and the pairing dies before
                    // it starts. Cost a real debugging session to find, and pinned by a test now.
                    .put("caps", capsArray()),
            )

            // 2. CHALLENGE.
            val challenge = receive(input)
            if (challenge.optString("type") != "CHALLENGE") {
                throw IllegalStateException("expected CHALLENGE, got ${challenge.optString("type")}")
            }
            // The proof covers the challenge's *raw* bytes, not its hex text. Decoding it here is the
            // difference between a pairing and a proof that is self-consistent and rejected.
            val nonce = challenge.optString("nonce").hexToBytes()
                ?: throw IllegalStateException("CHALLENGE carried no usable nonce")
            if (nonce.size != Crypto.CHALLENGE_LEN) {
                throw IllegalStateException(
                    "CHALLENGE nonce is ${nonce.size} bytes, not ${Crypto.CHALLENGE_LEN}",
                )
            }

            // 3. PAIR. The session id is the phone's to choose, and fresh for every connection: it
            //    selects the session key, which is what makes "the audio nonce is just seq" safe even
            //    though a restarted phone begins again at zero.
            val sessionId = ByteArray(SESSION_ID_LEN).also { SecureRandom().nextBytes(it) }
            send(
                out,
                JSONObject()
                    .put("type", "PAIR")
                    .put("proof", Crypto.proofHex(link.key, nonce, sessionId))
                    .put("session", sessionId.toHex()),
            )

            // 4. CONFIG, or a refusal.
            val answer = receive(input)
            when (answer.optString("type")) {
                "CONFIG" -> return Paired(
                    sessionKey = Crypto.sessionKey(link.key, sessionId),
                    config = Config(
                        sampleRate = answer.optInt("sample_rate", 48_000),
                        channels = answer.optInt("channels", 1),
                        format = answer.optInt("format", 1),
                        frameMs = answer.optInt("frame_ms", 10),
                        source = answer.optString("source", "unprocessed"),
                    ),
                    connection = Connection(socket, input, out),
                )

                "REJECTED" -> throw Rejected(
                    answer.optString("reason", "the host refused this phone's pairing key"),
                )

                else -> throw IllegalStateException(
                    "expected CONFIG or REJECTED, got ${answer.optString("type")}",
                )
            }
        } catch (e: Exception) {
            // Nothing owns the socket yet at this point, so this is the only place that can close it.
            runCatching { socket.close() }
            throw e
        }
    }

    /** What this phone can do, as a real JSON array of strings. */
    private fun capsArray(): JSONArray = JSONArray().put("aes-gcm").put("pcm-f32")

    /** Writes one newline-delimited line, and flushes it. */
    private fun send(out: Writer, message: JSONObject) {
        out.write(message.toString())
        out.write("\n")
        // A message left in a buffer is a handshake that never happens, and it would be diagnosed as
        // a network fault rather than as a missing flush.
        out.flush()
    }

    /** Reads one newline-delimited line. */
    private fun receive(input: BufferedReader): JSONObject {
        val line = input.readLine() ?: throw IllegalStateException("the host closed the connection")
        return JSONObject(line)
    }
}
