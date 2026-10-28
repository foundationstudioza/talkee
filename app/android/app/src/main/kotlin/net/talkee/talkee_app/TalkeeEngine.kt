package net.talkee.talkee_app

import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.util.Log
import android.os.SystemClock
import org.json.JSONObject
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.abs

/**
 * The entire phone-side pipeline: find a host, capture the microphone, send.
 *
 * # Why this is a singleton
 *
 * Both the Flutter plugin and the foreground service need the same pipeline, and
 * only one thing in a process may hold the microphone. Making this an `object`
 * means exactly one place owns the [AudioRecord] and the socket, so "start from a
 * cold app" and "keep running with the screen off" cannot race into two capture
 * streams.
 *
 * # Why this file is not Rust
 *
 * The committed architecture puts audio in a shared Rust core, and this belongs
 * there once Opus lands — the codec is what makes a shared implementation worth
 * the cross-compilation cost. For M0 the goal is to *measure real latency*, so the
 * framing below is a deliberately throwaway 24-byte writer implementing exactly
 * the layout in `docs/PROTOCOL.md`. It is the reference for the Rust
 * implementation, not a duplicate of it.
 */
object TalkeeEngine {

    private const val TAG = "Talkee"

    /** mDNS service type; the trailing `.local.` is added by [NsdManager]. */
    const val SERVICE_TYPE = "_talkee._tcp"

    // --- Wire format. Must match rust/talkee-proto exactly. ---
    private const val MAGIC = 0x4B54
    private const val PROTOCOL_VERSION = 1
    private const val HEADER_LEN = 24
    private const val FMT_F32 = 1
    private const val FMT_OPUS = 2

    /** Header flag: the payload is `[nonce][ciphertext][tag]` rather than plain PCM. */
    private const val FLAG_ENCRYPTED = 0x10

    // --- Audio format. 48 kHz mono f32 is what the desktop re-buffers, and what
    // WASAPI and CoreAudio both natively prefer. ---
    private const val SAMPLE_RATE = 48_000
    private const val FRAME_MS = 10
    private const val FRAME_SAMPLES = SAMPLE_RATE * FRAME_MS / 1000 // 480
    private const val BYTES_PER_SAMPLE = 4

    /**
     * How long to wait before trying a remembered host again.
     *
     * Three seconds, because the usual reason a pairing attempt fails is that the PC is waking up or
     * the Wi-Fi is still associating — both of which resolve in seconds, and neither of which is
     * worth telling the user about.
     */
    private const val RETRY_INTERVAL_MS = 3_000L

    // --- TXT record keys, from docs/PROTOCOL.md. ---
    private const val TXT_AUDIO_PORT = "ap"
    private const val TXT_VERSION = "v"

    // --- States, surfaced to the UI. ---
    const val STATE_IDLE = "idle"
    const val STATE_DISCOVERING = "discovering"
    const val STATE_STREAMING = "streaming"
    const val STATE_ERROR = "error"

    @Volatile private var state: String = STATE_IDLE
    @Volatile private var hostLabel: String? = null
    @Volatile private var hostAddress: String? = null
    @Volatile private var sourceLabel: String? = null
    @Volatile private var lastError: String? = null

    /** Peak level of the most recent frame, 0..1, for the UI meter. */
    @Volatile var level: Float = 0f
        private set

    // --- The desktop's switches, as it last told us. The desktop is the single source of truth: the
    // phone asks for a change and shows what comes back, so the two can never disagree. ---
    @Volatile private var paused = false
    @Volatile private var noiseSuppression = true
    @Volatile private var echoCancel = true

    /** Sends control messages off the UI thread, in order. */
    private val sender = java.util.concurrent.Executors.newSingleThreadExecutor { r ->
        Thread(r, "talkee-control-send").apply { isDaemon = true }
    }

    @Volatile private var packetsSent: Long = 0
    @Volatile private var samplesSent: Long = 0

    /**
     * The pairing in force, or null when streaming in the clear.
     *
     * Non-null means every frame is sealed *and* a socket is being held open: the host drops the
     * session the moment that control connection closes, so simply having this object is what keeps
     * the phone paired.
     */
    @Volatile private var pairing: Control.Paired? = null

    /** The link this run is using, so a reconnect pairs again rather than rediscovering. */
    @Volatile private var link: PairLink? = null

    private val running = AtomicBoolean(false)
    private var captureThread: Thread? = null

    /**
     * The thread that retries a remembered host.
     *
     * Held so [`stop`] can interrupt it. Without the interrupt a stopped engine keeps trying to
     * connect, and a phone that has been paused goes on looking busy.
     */
    @Volatile private var retryThread: Thread? = null

    /**
     * The thread that pairs with a remembered host, or discovers one.
     *
     * Separate from the capture thread because it outlives it: it keeps retrying while there is
     * nothing to stream from, and it must never run on the main thread.
     */
    private var connectThread: Thread? = null
    private var nsdManager: NsdManager? = null
    private var discoveryListener: NsdManager.DiscoveryListener? = null

    /** A snapshot for the UI. Cheap enough to poll several times a second. */
    fun status(): Map<String, Any?> = mapOf(
        "state" to state,
        "host" to hostLabel,
        "address" to hostAddress,
        "source" to sourceLabel,
        "level" to level,
        "packetsSent" to packetsSent,
        "samplesSent" to samplesSent,
        // Whether frames are sealed. Distinct from "host", which mDNS can also fill in: a phone that
        // found a host by discovery is streaming in the clear, and the UI has to be able to say so.
        "paired" to (pairing != null),
        // Whether this phone has a link at all, connected or not.
        //
        // Distinct from `paired`: a paired phone whose PC is asleep has a link and no session, and
        // the UI has to tell that apart from a phone that has never been paired. Showing a scanner
        // to somebody who has already scanned is what makes an app feel broken.
        "hasLink" to (link != null),
        "paused" to paused,
        "update" to Updater.version.takeIf { Updater.state == "ready" },
        "updating" to (Updater.state == "downloading"),
        "noiseSuppression" to noiseSuppression,
        "echoCancel" to echoCancel,
        "error" to lastError,
    )

    /**
     * Finds a host and starts streaming. Idempotent: calling it while already
     * running does nothing, so the UI can call it freely on every resume without
     * the user ever having to ask.
     */
    @Synchronized
    fun start(context: Context) {
        if (running.get()) return
        lastError = null
        Log.i(TAG, "engine start")

        if (context.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            fail("microphone permission not granted")
            return
        }

        running.set(true)
        // Held only so the capture loop can resume discovery after a drop. An
        // application context is correct here: this singleton lives as long as
        // the process, and holding an Activity would leak it.
        appContext = context.applicationContext
        state = STATE_DISCOVERING
        hostLabel = null
        hostAddress = null

        // A stored link first. It carries the address as well as the key, so a paired phone needs no
        // discovery at all — which is also what makes pairing work on the networks where mDNS is
        // blocked: guest Wi-Fi, client isolation, a VPN, or a phone and a PC on different subnets.
        //
        // On its own thread, and that is not a detail: `start` is called from the foreground
        // service's `onCreate`, which is the *main* thread, and connecting to a PC that is asleep
        // takes as long as its timeout allows. Doing it inline froze the app on launch whenever the
        // PC was not running — which reads to a user as a crash.
        val remembered = PairingStore.load(context)?.let { PairLink.parse(it) }
        if (remembered != null) {
            // Recorded before the thread starts, so the UI knows from the first status push that this
            // phone is paired and should not be shown a scanner.
            link = remembered
        }
        val worker = Thread({
            if (remembered != null) {
                retryLink(remembered)
            } else {
                discover(context)
            }
        }, "talkee-connect")
        connectThread = worker
        worker.start()
    }

    /**
     * Keeps trying a remembered host until one answers or the engine stops.
     *
     * Deliberately *not* a fallback to discovery. A phone holding a link is paired, and quietly
     * finding some other host — or advertising itself as available for plaintext — would turn "my PC
     * is asleep" into "my phone is sending audio nobody will accept", which is a much harder thing
     * for a person to understand. It retries instead, and says only that it is looking.
     *
     * Runs on `talkee-connect`. Never call it from the main thread.
     */
    private fun retryLink(initial: PairLink) {
        var current = initial
        var failures = 0
        wake.drainPermits()
        while (running.get()) {
            if (pairAndStream(current)) {
                return
            }
            failures++
            // Two misses in a row and the address itself may be what is wrong: routers hand them out
            // afresh, so look for the PC by its key's fingerprint and follow it to where it is now.
            if (failures >= 2) {
                val context = appContext
                val moved = if (context != null) HostFinder.find(context, current.fingerprint) else null
                val address = moved?.hostAddress
                if (address != null && address != current.host) {
                    Log.i(TAG, "the PC moved from ${current.host} to $address")
                    current = current.withHost(address)
                    PairingStore.save(context!!, current.toUri())
                    this.link = current
                    failures = 0
                    continue
                }
            }
            state = STATE_DISCOVERING
            hostLabel = null
            hostAddress = null
            try {
                // Woken early by [reconnectNow], so a tap on "retry" tries straight away.
                wake.tryAcquire(RETRY_INTERVAL_MS, java.util.concurrent.TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
                return
            }
        }
    }

    /** Wakes the retry loop out of its pause. */
    private val wake = java.util.concurrent.Semaphore(0)

    /**
     * Drops whatever connection there is and pairs again, now.
     *
     * For a connection that looks alive and carries nothing, and for the "try again" tap: it does what
     * the loop would have done on its own a few seconds later, without the wait. Starts the engine if
     * it is not running.
     */
    fun reconnectNow(context: Context) {
        if (!running.get()) {
            CaptureService.start(context)
            return
        }
        pairing?.connection?.close()
        wake.release()
    }

    /** Stops streaming and discovery. Safe to call when already stopped. */
    @Synchronized
    fun stop() {
        running.set(false)
        stopDiscovery()
        // Interrupted rather than waited for: the connect thread is usually asleep in a retry, and a
        // stop that hangs for three seconds is a stop the user does not believe happened.
        connectThread?.interrupt()
        connectThread = null
        captureThread?.join(500)
        captureThread = null
        // Closing this is what unpairs the phone on the host: the session there lives only as long
        // as this connection does, so a stopped phone stops being able to send accepted audio.
        pairing?.connection?.close()
        pairing = null
        link = null
        paused = false
        state = STATE_IDLE
        level = 0f
    }

    /**
     * Asks the desktop to change a setting: `paused`, `noise_suppression` or `echo_cancel`.
     *
     * Nothing changes here. The desktop applies it and answers with its new state, which is what the
     * screen shows — so a request that did not arrive leaves the screen telling the truth.
     */
    fun setSetting(key: String, value: Boolean) {
        val connection = pairing?.connection ?: return
        sender.execute {
            try {
                connection.send(JSONObject().put("type", "SET").put(key, value))
            } catch (e: Exception) {
                Log.w(TAG, "could not send $key=$value: $e")
            }
        }
    }

    /**
     * Pairs with `link` and starts streaming to it. Returns false when that did not work.
     *
     * A failure is deliberately *not* an error state. The caller falls back to discovery, because a
     * phone whose PC has rotated its key, or whose stored link is stale, should still find the host
     * and stream — reporting why the pairing failed is a bonus, not a precondition.
     */
    /**
     * Pairs with `link` and starts streaming to it. Returns false when that did not work.
     *
     * A failure here is deliberately *not* an error state, and deliberately does not touch
     * [lastError]. This runs on its own every time the app starts and every time a stream drops, so
     * the overwhelmingly common failure is simply that the PC is asleep, starting up, or on the other
     * side of a Wi-Fi roam — none of which is something the user did, and none of which deserves to
     * put a message on their screen. The caller retries; the UI is told nothing.
     */
    private fun pairAndStream(link: PairLink): Boolean {
        Log.i(TAG, "pairing with ${link.host}:${link.controlPort}")
        val paired = try {
            Control.pair(link, deviceId(), deviceLabel())
        } catch (e: Exception) {
            Log.w(TAG, "pairing with ${link.host}:${link.controlPort} failed: $e")
            return false
        }
        Log.i(TAG, "paired with ${link.host}; streaming audio to port ${link.audioPort}")

        val address = try {
            InetAddress.getByName(link.host)
        } catch (e: Exception) {
            paired.connection.close()
            return false
        }

        pairing?.connection?.close()
        pairing = paired
        paired.connection.watch { p, n, e ->
            paused = p
            noiseSuppression = n
            echoCancel = e
        }
        this.link = link
        lastError = null
        hostLabel = link.name
        // The host tells us how it wants the audio; report what it asked for.
        sourceLabel = paired.config.source
        hostAddress = "${link.host}:${link.audioPort}"
        startCapture(address, link.audioPort)
        return true
    }

    /**
     * Pairs with `link` because the user asked, and says why if it did not work.
     *
     * The counterpart to the quiet path above. A scan is the one time a pairing failure is the
     * user's business: they just did something deliberate and are owed an answer.
     */
    fun pairNow(context: Context, link: PairLink): String? {
        stop()
        running.set(true)
        appContext = context
        if (pairAndStream(link)) {
            return null
        }
        // Connect once more purely to get a reason to show: `pairAndStream` swallows it on purpose,
        // and re-deriving it here keeps that function free of "who is asking" logic.
        return try {
            Control.pair(link, deviceId(), deviceLabel()).connection.close()
            // It worked on the second attempt, so try the whole thing once more.
            if (pairAndStream(link)) null else "could not start streaming to ${link.name}"
        } catch (e: Exception) {
            friendly(e)
        }
    }

    /** Turns a connection failure into a sentence worth reading. */
    private fun friendly(e: Exception): String = when (e) {
        is Control.Rejected -> "the PC did not recognise this code (${e.message})"
        is java.net.SocketTimeoutException -> "no answer from ${hostAddress ?: "the PC"}"
        is java.net.ConnectException -> "nothing is listening at ${hostAddress ?: "the PC"}"
        is java.net.UnknownHostException -> "could not find ${link?.host ?: "the PC"}"
        else -> e.message ?: e.javaClass.simpleName
    }

    /**
     * A stable id for this install.
     *
     * The model name rather than a generated id: it is stable, it is what the desktop shows next to
     * a pairing, and it tells an attacker nothing they could not read off the device. A truly
     * per-install id would be another secret to protect for no gain.
     */
    private fun deviceId(): String = (Build.MODEL ?: "android").lowercase().replace(' ', '-')

    /** What the desktop shows in its own list. */
    private fun deviceLabel(): String =
        listOfNotNull(Build.MANUFACTURER?.takeIf { it.isNotBlank() }, Build.MODEL)
            .joinToString(" ")
            .trim()
            .ifBlank { "Android phone" }

    private fun fail(message: String) {
        lastError = message
        state = STATE_ERROR
        running.set(false)
        stopDiscovery()
    }

    // ---------------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------------

    /**
     * Browses for a host.
     *
     * Uses [NsdManager] rather than doing mDNS ourselves, because raw multicast
     * on Android needs a held `WifiManager.MulticastLock` to stop the radio
     * filtering packets — a classic silent-failure trap that the platform API
     * handles for us.
     */
    private fun discover(context: Context) {
        val nsd = context.getSystemService(Context.NSD_SERVICE) as? NsdManager
        if (nsd == null) {
            fail("this device has no network service discovery")
            return
        }
        nsdManager = nsd

        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) = Unit

            override fun onServiceFound(service: NsdServiceInfo) {
                resolve(service)
            }

            override fun onServiceLost(service: NsdServiceInfo) {
                // Losing the host we stream to is detected by the capture loop's
                // own send failures, which can then reconnect. Nothing here.
            }

            override fun onDiscoveryStopped(serviceType: String) = Unit

            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                fail("could not search for your PC (error $errorCode)")
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit
        }
        discoveryListener = listener

        try {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
        } catch (e: Exception) {
            fail("could not start discovery: ${e.message}")
        }
    }

    private fun stopDiscovery() {
        val nsd = nsdManager ?: return
        val listener = discoveryListener ?: return
        discoveryListener = null
        try {
            nsd.stopServiceDiscovery(listener)
        } catch (_: Exception) {
            // Stopping discovery that has already stopped throws on some builds.
        }
    }

    /**
     * Resolves a found service to an address.
     *
     * `NsdManager` permits only **one** resolve at a time and calls
     * `onResolveFailed` with `FAILURE_ALREADY_ACTIVE` if you break that rule.
     * Since `onServiceFound` can fire in bursts, resolutions are serialised here
     * with a single pending slot: an in-flight resolve queues at most one
     * successor, and the queue is drained on completion *or* failure. Without
     * this, discovery works right up until a second device appears — or up until
     * the first attempt fails — and then goes permanently silent.
     */
    private val resolving = AtomicBoolean(false)
    @Volatile private var pendingResolve: NsdServiceInfo? = null

    private fun resolve(service: NsdServiceInfo) {
        val nsd = nsdManager ?: return
        if (!resolving.compareAndSet(false, true)) {
            pendingResolve = service
            return
        }
        try {
            nsd.resolveService(service, object : NsdManager.ResolveListener {
                override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                    // Not fatal: a burst of resolves often fails a few before one
                    // succeeds, and the service is re-announced periodically.
                    resolving.set(false)
                    drainPending()
                }

                override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                    resolving.set(false)
                    val address = serviceInfo.host
                    if (address != null && running.get() && state != STATE_STREAMING) {
                        onHostFound(serviceInfo, address)
                    }
                    drainPending()
                }
            })
        } catch (e: Exception) {
            resolving.set(false)
            drainPending()
        }
    }

    private fun drainPending() {
        val next = pendingResolve ?: return
        pendingResolve = null
        // Only chase more hosts if we still need one.
        if (running.get() && state != STATE_STREAMING) resolve(next)
    }

    /**
     * Reads the audio port out of the TXT record, falling back to the SRV port.
     *
     * A service has only one SRV port, and we need two (control and audio), so
     * the audio port travels in TXT. The fallback keeps a host that published
     * nothing usable reachable rather than producing port 0.
     */
    private fun audioPortOf(serviceInfo: NsdServiceInfo): Int {
        val raw = serviceInfo.attributes?.get(TXT_AUDIO_PORT)?.toString(Charsets.UTF_8)
        return raw?.toIntOrNull()?.takeIf { it in 1..65535 } ?: serviceInfo.port
    }

    // ---------------------------------------------------------------------
    // Capture
    // ---------------------------------------------------------------------

    /** Application context, only so the capture loop can resume discovery. */
    @Volatile private var appContext: Context? = null

    /** Guards against two capture threads when discovery resolves in a burst. */
    private val starting = AtomicBoolean(false)

    private fun onHostFound(serviceInfo: NsdServiceInfo, address: InetAddress) {
        val port = audioPortOf(serviceInfo)
        hostLabel = serviceInfo.serviceName
        hostAddress = "${address.hostAddress}:$port"
        startCapture(address, port)
    }

    /**
     * Starts the capture thread, unless one is already starting.
     *
     * A resolve burst can deliver several hosts at once, and only the first may claim the
     * microphone — which is what the flag is for, and why this is the single door into streaming.
     */
    private fun startCapture(address: InetAddress, port: Int) {
        if (!starting.compareAndSet(false, true)) return
        if (!running.get()) {
            starting.set(false)
            return
        }
        stopDiscovery()
        val thread = Thread({ captureLoop(address, port) }, "talkee-capture")
        captureThread = thread
        // Audio must not be preempted by UI work.
        thread.priority = Thread.MAX_PRIORITY
        thread.start()
    }

    /**
     * Opens the microphone, preferring a source that gives us the signal
     * unmodified.
     *
     * [MediaRecorder.AudioSource.UNPROCESSED] bypasses automatic gain control,
     * noise suppression and echo cancellation, which is what "use my phone as a
     * microphone" actually means. Not every device honours it, so
     * [MediaRecorder.AudioSource.VOICE_RECOGNITION] and finally
     * [MediaRecorder.AudioSource.MIC] are tried in turn: a slightly processed
     * microphone beats silence, and the chosen source is reported to the UI so
     * that a surprising result is diagnosable rather than mysterious.
     */
    private fun openRecorder(): Triple<AudioRecord, Int, String>? {
        val minBuffer = AudioRecord.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_FLOAT,
        )
        if (minBuffer <= 0) return null

        // Ask for several frames' worth, so a scheduling hiccup does not drop
        // audio before our loop even runs.
        val bufferBytes = maxOf(minBuffer, FRAME_SAMPLES * BYTES_PER_SAMPLE * 8)
        val candidates = listOf(
            MediaRecorder.AudioSource.UNPROCESSED to "unprocessed",
            MediaRecorder.AudioSource.VOICE_RECOGNITION to "voice-recognition",
            MediaRecorder.AudioSource.MIC to "mic",
        )
        for ((source, label) in candidates) {
            val recorder = try {
                AudioRecord(
                    source,
                    SAMPLE_RATE,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_FLOAT,
                    bufferBytes,
                )
            } catch (_: Exception) {
                continue
            }
            if (recorder.state == AudioRecord.STATE_INITIALIZED) {
                return Triple(recorder, source, label)
            }
            recorder.release()
        }
        return null
    }

    /** Consecutive send failures before giving up and looking for the host again. */
    private val sendFailureLimit = 20

    private fun captureLoop(address: InetAddress, port: Int) {
        val opened = openRecorder()
        if (opened == null) {
            starting.set(false)
            fail("could not open the microphone at 48 kHz")
            return
        }
        val (recorder, _, label) = opened
        sourceLabel = label

        val frame = FloatArray(FRAME_SAMPLES)
        // Compressed audio when the PC asked for it and this phone can. If the codec will not start the
        // phone falls back to raw samples, which the PC reads just as well: every datagram says which
        // it is.
        val wantsOpus = pairing?.config?.format == FMT_OPUS
        val encoder = if (wantsOpus) OpusEncoder.create(pairing?.config?.bitrate?.takeIf { it > 0 } ?: 64_000) else null
        if (wantsOpus && encoder == null) Log.w(TAG, "no Opus encoder; sending raw samples")
        var socket: DatagramSocket? = null
        var failures = 0
        var seq = 0
        val startedAt = SystemClock.elapsedRealtime()

        try {
            socket = DatagramSocket().also { it.connect(address, port) }
            recorder.startRecording()
            state = STATE_STREAMING

            while (running.get()) {
                // READ_BLOCKING paces this loop to the microphone's real clock,
                // which is also what stops us sending faster than real time.
                if (pairing?.connection?.lost == true) {
                    Log.w(TAG, "the PC closed the control connection; pairing again")
                    lastError = "the PC went away"
                    break
                }
                if (paused) {
                    // Muted from the PC or from here. The microphone is released, not merely
                    // ignored: the system's mic indicator goes out, which is the honest signal.
                    runCatching { recorder.stop() }
                    level = 0f
                    while (paused && running.get() && pairing?.connection?.lost != true) {
                        Thread.sleep(60)
                    }
                    if (running.get() && !paused) {
                        runCatching { recorder.startRecording() }
                        // Audio the codec still held belongs to before the mute.
                        encoder?.reset()
                    }
                    continue
                }
                val read = recorder.read(frame, 0, FRAME_SAMPLES, AudioRecord.READ_BLOCKING)
                if (read <= 0) {
                    if (read == AudioRecord.ERROR_INVALID_OPERATION ||
                        read == AudioRecord.ERROR_BAD_VALUE
                    ) {
                        lastError = "microphone read failed"
                        break
                    }
                    continue
                }

                var peak = 0f
                for (i in 0 until read) {
                    val magnitude = abs(frame[i])
                    if (magnitude > peak) peak = magnitude
                }
                level = peak

                // One 10 ms frame becomes one raw datagram or, through the codec, nothing yet or a
                // packet of 20 ms. Either way `seq` counts datagrams, which is what the PC's loss
                // detection needs, and each datagram's header says which of the two it is.
                val now = (SystemClock.elapsedRealtime() - startedAt).toInt()
                val datagrams: List<Pair<ByteArray, Int>> = if (encoder != null) {
                    encoder.encode(frame, read).mapIndexed { n, packet ->
                        buildOpusPacket(seq + n, now, packet) to packet.samples
                    }
                } else {
                    listOf(buildPacket(seq = seq, tsMs = now, samples = read, pcm = frame) to read)
                }
                for ((packet, samples) in datagrams) {
                    try {
                        socket.send(DatagramPacket(packet, packet.size))
                        packetsSent++
                        samplesSent += samples
                        failures = 0
                    } catch (_: Exception) {
                        // On a Windows peer, a dead host surfaces as ICMP port
                        // unreachable rather than silence. A run of these means the
                        // host is gone, not that this one frame was unlucky.
                        failures++
                        if (failures >= sendFailureLimit) {
                            lastError = "lost the connection to your PC"
                            break
                        }
                    }
                    seq++
                }
                if (failures >= sendFailureLimit) break
            }
        } catch (e: Exception) {
            lastError = "capture failed: ${e.message}"
        } finally {
            runCatching { recorder.stop() }
            recorder.release()
            encoder?.close()
            runCatching { socket?.close() }
            level = 0f
            starting.set(false)

            // If we stopped because something broke rather than because the user
            // asked us to, go back to looking for the host. This is what lets the
            // app survive the PC sleeping, a Wi-Fi roam, or the desktop being
            // restarted — with no user involvement whatsoever.
            if (running.get()) {
                state = STATE_DISCOVERING
                hostLabel = null
                hostAddress = null
                // A paired phone goes straight back to the host it already knows rather than
                // searching for one: the link it holds is the whole reason discovery is optional.
                val remembered = link
                if (remembered != null) {
                    // Keeps trying the PC it knows, on its own thread so this one can finish. Never
                    // falls back to discovery: see `retryLink`.
                    pairing?.connection?.close()
                    pairing = null
                    val worker = Thread({ retryLink(remembered) }, "talkee-connect")
                    connectThread = worker
                    worker.start()
                } else {
                    val context = appContext
                    if (context != null) {
                        discover(context)
                    } else {
                        running.set(false)
                    }
                }
            }
        }
    }

    /**
     * Serialises one audio datagram.
     *
     * Little-endian, 24-byte header, exactly as specified in `docs/PROTOCOL.md`.
     * Correctness is verified end-to-end by the desktop: a byte out of place
     * would present as *zero* packets received, because the Rust parser rejects
     * a header whose declared payload length disagrees with its fields.
     */
    /**
     * Serialises one datagram carrying an Opus packet.
     *
     * Always sealed: Opus is only ever asked for in a pairing, and a packet that is not authentic is
     * refused by the PC before the decoder sees it. The payload length is the packet plus the nonce
     * and tag, and the header is the associated data, exactly as for raw samples.
     */
    private fun buildOpusPacket(seq: Int, tsMs: Int, packet: OpusEncoder.Packet): ByteArray {
        val session = requireNotNull(pairing) { "Opus is only sent in a pairing" }
        val payloadLen = packet.bytes.size + Crypto.AEAD_OVERHEAD
        val buffer = ByteBuffer.allocate(HEADER_LEN + payloadLen).order(ByteOrder.LITTLE_ENDIAN)
        buffer.putShort(MAGIC.toShort())
        buffer.put(PROTOCOL_VERSION.toByte())
        buffer.put(FLAG_ENCRYPTED.toByte())
        buffer.putInt(seq)
        buffer.putInt(tsMs)
        buffer.putInt(SAMPLE_RATE)
        buffer.putShort(packet.samples.toShort())
        buffer.put(1.toByte()) // channels: mono
        buffer.put(FMT_OPUS.toByte())
        buffer.putInt(payloadLen)
        val header = buffer.array().copyOfRange(0, HEADER_LEN)
        buffer.put(Crypto.seal(session.sessionKey, Crypto.PHONE_TO_DESKTOP, seq, packet.bytes, header))
        return buffer.array()
    }

    private fun buildPacket(seq: Int, tsMs: Int, samples: Int, pcm: FloatArray): ByteArray {
        val plainBytes = samples * BYTES_PER_SAMPLE
        val session = pairing
        // A sealed payload is the plaintext plus a nonce and a tag, and the header says so: the
        // receiver checks `payload_len` against the sample count and the flag, so a frame that
        // declared one without the other is rejected rather than misparsed.
        val payloadLen = if (session == null) plainBytes else plainBytes + Crypto.AEAD_OVERHEAD

        val buffer = ByteBuffer.allocate(HEADER_LEN + payloadLen).order(ByteOrder.LITTLE_ENDIAN)
        buffer.putShort(MAGIC.toShort())
        buffer.put(PROTOCOL_VERSION.toByte())
        buffer.put(if (session == null) 0.toByte() else FLAG_ENCRYPTED.toByte())
        buffer.putInt(seq)
        buffer.putInt(tsMs)
        buffer.putInt(SAMPLE_RATE)
        buffer.putShort(samples.toShort())
        buffer.put(1.toByte()) // channels: mono
        buffer.put(FMT_F32.toByte())
        buffer.putInt(payloadLen)

        if (session == null) {
            for (i in 0 until samples) buffer.putFloat(pcm[i])
        } else {
            // Two short-lived arrays the plain path does not need, because GCM works on bytes rather
            // than floats. At a hundred frames a second that is two hundred small allocations, which
            // the young generation absorbs — noted so a future reader knows it was considered rather
            // than overlooked.
            val plain = ByteArray(plainBytes)
            ByteBuffer.wrap(plain).order(ByteOrder.LITTLE_ENDIAN).asFloatBuffer().put(pcm, 0, samples)
            // The header is the associated data: the host opens with the same 24 bytes, so every
            // field in it is authenticated along with the audio. It has been fully written by now.
            val header = buffer.array().copyOfRange(0, HEADER_LEN)
            // The sequence number is the nonce, so it has to be the same value the header carries.
            buffer.put(Crypto.seal(session.sessionKey, Crypto.PHONE_TO_DESKTOP, seq, plain, header))
        }
        return buffer.array()
    }
}


