package net.talkee.talkee_app

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine

/**
 * The only Android entry point.
 *
 * Two jobs beyond hosting Flutter: register the plugin, and get the microphone
 * permission granted without the user having to hunt for a setting. Once granted,
 * streaming is started from here rather than waiting for Dart, so the result is
 * the same whether the user grants permission on first launch or later.
 */
class MainActivity : FlutterActivity() {

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        flutterEngine.plugins.add(TalkeePlugin())
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        // Before `super.onCreate`, deliberately. `super` starts the Flutter engine, Dart starts the
        // audio engine as soon as it has a UI, and the engine reads the pairing store when it starts
        // — so saving afterwards is a race whose losing side is a phone that streams in the clear.
        rememberLinkFrom(intent)
        super.onCreate(savedInstanceState)
        ensurePermissions()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // A single-top activity gets a *new* intent rather than a new instance, so this is the only
        // path a second scan takes while the app is already open.
        rememberLinkFrom(intent)
    }

    /**
     * Stores the link this activity was launched with, and starts streaming to it.
     *
     * Refuses anything that is not a Talkee link rather than storing something that would fail
     * silently later. The UI learns that pairing worked the same way it learns everything else: the
     * status stream starts reporting a host.
     */
    private fun rememberLinkFrom(intent: Intent?) {
        val text = intent?.getStringExtra(EXTRA_LINK) ?: intent?.dataString ?: return
        if (PairLink.parse(text) == null) return
        PairingStore.save(this, text)
        if (!hasMicrophone()) return
        // Stop before starting. The engine may already be running — Dart starts it on launch, and a
        // second scan can arrive at any moment — and `TalkeeEngine.start` is a no-op while it is.
        // Restarting is what makes a newly scanned link take effect, including a different PC.
        TalkeeEngine.stop()
        CaptureService.start(this)
        // The service's `onCreate` is what starts the engine, and it does not run again when the
        // service is already up — which is exactly the case here, because Dart started it on launch.
        // Without this the engine was stopped above and never restarted: a link that arrived while
        // the app was open was saved and then ignored.
        TalkeeEngine.start(applicationContext)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != PERMISSION_REQUEST) return
        // Starting here, rather than relying on Dart to retry, is what makes the
        // first-run flow a single tap with no further interaction.
        if (hasMicrophone()) {
            CaptureService.start(this)
        }
    }

    private fun ensurePermissions() {
        val needed = mutableListOf<String>()
        if (!hasMicrophone()) {
            needed += Manifest.permission.RECORD_AUDIO
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            // Without this the live notification is invisible on Android 13+,
            // which hides the fact that the microphone is on.
            needed += Manifest.permission.POST_NOTIFICATIONS
        }
        if (needed.isNotEmpty()) {
            requestPermissions(needed.toTypedArray(), PERMISSION_REQUEST)
        }
    }

    private fun hasMicrophone(): Boolean =
        checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    private companion object {
        const val PERMISSION_REQUEST = 4711

        /**
         * Where a pairing link arrives from outside.
         *
         * `adb shell am start -n net.talkee.talkee_app/.MainActivity --es talkee_link "<link>"`
         * during a test, and a QR scanner's result in normal use.
         */
        const val EXTRA_LINK = "talkee_link"
    }
}

