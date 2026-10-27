package net.talkee.talkee_app

import android.content.Context
import android.os.Handler
import android.os.Looper
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel

/**
 * Bridges [TalkeeEngine] to Dart.
 *
 * Deliberately tiny. The Dart side has exactly four verbs — start, stop, status, pair — because the
 * entire product promise is that the user does not configure anything. Any decision that would need
 * a fifth verb belongs in the engine.
 *
 * `pair` is the one that comes from the user rather than from the app's own lifecycle: a scanned
 * code. Everything else before a scan is machinery.
 *
 * Status is *pushed* on a 250 ms timer rather than pulled. Polling from Dart would
 * work too, but a timer here means the UI cannot accidentally leave the
 * microphone running with nothing observing it, and 4 Hz is far more than a
 * level meter needs.
 */
class TalkeePlugin : FlutterPlugin, MethodChannel.MethodCallHandler, EventChannel.StreamHandler {

    private lateinit var context: Context
    private lateinit var methods: MethodChannel
    private lateinit var statusChannel: EventChannel
    private var sink: EventChannel.EventSink? = null

    private val handler = Handler(Looper.getMainLooper())
    private val pump = object : Runnable {
        override fun run() {
            sink?.success(TalkeeEngine.status())
            handler.postDelayed(this, STATUS_INTERVAL_MS)
        }
    }

    override fun onAttachedToEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        context = binding.applicationContext
        methods = MethodChannel(binding.binaryMessenger, METHODS)
        methods.setMethodCallHandler(this)
        statusChannel = EventChannel(binding.binaryMessenger, STATUS)
        statusChannel.setStreamHandler(this)
    }

    override fun onDetachedFromEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        handler.removeCallbacks(pump)
        methods.setMethodCallHandler(null)
        statusChannel.setStreamHandler(null)
        sink = null
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            // Start goes through the service, not the engine, so the foreground
            // notification and the process-priority bump always accompany it.
            "start" -> {
                CaptureService.start(context)
                result.success(null)
            }
            "stop" -> {
                CaptureService.stop(context)
                result.success(null)
            }
            "status" -> result.success(TalkeeEngine.status())
            // Drop the connection and pair again now. The retry tap and the "reconnect" control.
            "reconnect" -> {
                TalkeeEngine.reconnectNow(context)
                result.success(null)
            }
            // Look for a newer build. Quiet: the answer arrives in the status stream.
            "checkUpdate" -> {
                Updater.check(context)
                result.success(null)
            }
            // The user tapped "install". The system asks again before anything changes.
            "installUpdate" -> result.success(Updater.install(context))
            // A switch the user turned on this screen. Sent to the PC, which is what decides.
            "set" -> {
                val key = call.argument<String>("key")
                val value = call.argument<Boolean>("value")
                if (key == null || value == null) {
                    result.error("bad-args", "set needs a key and a value", null)
                } else {
                    TalkeeEngine.setSetting(key, value)
                    result.success(null)
                }
            }
            // A scanned pairing link, from the Dart scanner.
            //
            // The engine decides what to do with it; this only says whether it looks like a link at
            // all, because a camera can be pointed at anything and the UI needs a yes or a no to
            // leave the scanner or explain itself.
            "pair" -> {
                val link = call.argument<String>("link")
                val parsed = link?.let { PairLink.parse(it) }
                if (parsed == null) {
                    result.error("not-a-link", "that is not a Talkee pairing code", null)
                    return
                }
                PairingStore.save(context, link!!)
                // Paired off the main thread. `pairNow` does a TCP connect with a timeout, which is
                // up to ten seconds of blocking — on the main thread that is a frozen camera and an
                // unresponsive app at exactly the moment the user is looking at it.
                //
                // The answer still goes back to Dart, because a scan is the one pairing attempt that
                // is the user's business: they did something deliberate and are owed the outcome.
                val handler = Handler(Looper.getMainLooper())
                Thread({
                    val problem = TalkeeEngine.pairNow(context, parsed)
                    handler.post {
                        if (problem == null) {
                            CaptureService.start(context)
                            result.success(null)
                        } else {
                            result.error("pairing-failed", problem, null)
                        }
                    }
                }, "talkee-pair-scan").start()
            }
            else -> result.notImplemented()
        }
    }

    override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
        sink = events
        handler.post(pump)
    }

    override fun onCancel(arguments: Any?) {
        handler.removeCallbacks(pump)
        sink = null
    }

    private companion object {
        const val METHODS = "net.talkee/capture"
        const val STATUS = "net.talkee/status"
        const val STATUS_INTERVAL_MS = 250L
    }
}
