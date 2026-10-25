package net.talkee.talkee_app

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import org.json.JSONObject
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest

/**
 * Keeping the phone app current without the Play Store.
 *
 * # What it does
 *
 * Reads `update.json` from the latest GitHub release, and when it names a newer build downloads the
 * APK, checks its SHA-256, and tells the screen an update is ready. **Installing needs one tap from
 * the user, and that cannot be removed:** outside the Play Store, Android does not let an app replace
 * itself silently. The tap hands the file to the system installer, which asks for its own
 * confirmation.
 *
 * # Why a new build must be signed with the same key
 *
 * Android refuses to update an app with a build signed by a different key. Release builds are signed
 * with one stable key held in the repository's secrets; a build made on a developer machine with the
 * debug key cannot be updated this way, and has to be uninstalled once.
 */
object Updater {

    private const val TAG = "TalkeeUpdate"

    /** Latest release's manifest. GitHub redirects `latest` to the newest release, so this is fixed. */
    private const val MANIFEST_URL =
        "https://github.com/foundationstudioza/talkee/releases/latest/download/update.json"

    private const val MAX_APK_BYTES = 300L * 1024 * 1024

    /** `"downloading"`, `"ready"`, or null for nothing to do. */
    @Volatile var state: String? = null
        private set

    /** The version of the update that is downloading or ready. */
    @Volatile var version: String? = null
        private set

    @Volatile private var apk: File? = null
    @Volatile private var checking = false

    /** Looks for an update on a background thread. Safe to call as often as the screen likes. */
    fun check(context: Context) {
        if (checking || state == "ready") return
        checking = true
        val app = context.applicationContext
        Thread({
            try {
                run(app)
            } catch (e: Exception) {
                // Offline, rate-limited, a bad manifest: none of them is the user's problem.
                Log.i(TAG, "update check failed: $e")
                if (state == "downloading") state = null
            } finally {
                checking = false
            }
        }, "talkee-update").apply { isDaemon = true }.start()
    }

    private fun run(context: Context) {
        val manifest = JSONObject(fetchText(MANIFEST_URL))
        val android = manifest.optJSONObject("android") ?: return
        val code = android.optLong("versionCode", 0)
        val current = context.packageManager.getPackageInfo(context.packageName, 0).longVersionCode
        if (code <= current) return

        val url = android.optString("url")
        val sha = android.optString("sha256").lowercase()
        if (!url.startsWith("https://") || sha.length != 64) return

        version = manifest.optString("version").trimStart('v')
        state = "downloading"
        val dir = File(context.cacheDir, "update").apply { mkdirs() }
        dir.listFiles()?.forEach { it.delete() }
        val file = File(dir, "talkee-$code.apk")

        val digest = MessageDigest.getInstance("SHA-256")
        val connection = open(url, 120_000)
        var total = 0L
        connection.inputStream.use { input ->
            file.outputStream().use { out ->
                val buffer = ByteArray(64 * 1024)
                while (true) {
                    val read = input.read(buffer)
                    if (read < 0) break
                    total += read
                    if (total > MAX_APK_BYTES) throw IllegalStateException("the update is too large")
                    digest.update(buffer, 0, read)
                    out.write(buffer, 0, read)
                }
            }
        }
        val got = digest.digest().joinToString("") { "%02x".format(it) }
        if (got != sha) {
            file.delete()
            throw IllegalStateException("the download's SHA-256 does not match the release")
        }
        apk = file
        state = "ready"
        Log.i(TAG, "update $version is ready")
    }

    private fun open(url: String, readTimeoutMs: Int): HttpURLConnection {
        val connection = URL(url).openConnection() as HttpURLConnection
        connection.connectTimeout = 15_000
        connection.readTimeout = readTimeoutMs
        connection.instanceFollowRedirects = true
        return connection
    }

    private fun fetchText(url: String): String =
        open(url, 20_000).inputStream.bufferedReader().use { it.readText() }

    /**
     * Hands the downloaded APK to the system installer. Returns false when something stood in the way
     * (nothing downloaded, or the user has not allowed this app to install apps yet, in which case the
     * system settings page for that is opened).
     */
    fun install(context: Context): Boolean {
        val file = apk?.takeIf { state == "ready" && it.exists() } ?: return false
        val packageManager = context.packageManager
        if (!packageManager.canRequestPackageInstalls()) {
            // Android asks the user to allow this once per app. Send them to the right page.
            context.startActivity(
                Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${context.packageName}"))
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
            return false
        }
        val installer = packageManager.packageInstaller
        val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
        val session = installer.openSession(installer.createSession(params))
        session.use { s ->
            file.inputStream().use { input ->
                s.openWrite("talkee.apk", 0, file.length()).use { out ->
                    input.copyTo(out)
                    s.fsync(out)
                }
            }
            val intent = Intent(context, InstallReceiver::class.java)
            val flags = PendingIntent.FLAG_UPDATE_CURRENT or
                (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0)
            s.commit(PendingIntent.getBroadcast(context, 0, intent, flags).intentSender)
        }
        return true
    }
}

/** Receives the installer's answer, and shows its confirmation screen when it asks for one. */
class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.getIntExtra(PackageInstaller.EXTRA_STATUS, -1)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                @Suppress("DEPRECATION")
                val confirm = intent.getParcelableExtra<Intent>(Intent.EXTRA_INTENT)
                confirm?.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                if (confirm != null) context.startActivity(confirm)
            }
            else -> Log.i("TalkeeUpdate", "installer: ${intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)}")
        }
    }
}
