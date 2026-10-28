package net.talkee.talkee_app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.net.wifi.WifiManager
import android.os.IBinder
import android.os.PowerManager

/**
 * Keeps capture alive when the screen is off or the app is in the background.
 *
 * # Why a foreground service is mandatory rather than nice-to-have
 *
 * The product requirement is that the user never has to think about this app
 * again after opening it once. That means audio must survive the phone going into
 * a pocket. On Android 14 and later a *microphone* foreground service is the only
 * supported way to capture in the background, and it may **only be started while
 * the app is visible** — which is why the service is launched from the activity
 * (and from the permission callback), not from a background receiver.
 *
 * The notification is not optional either; Android requires it, and it doubles as
 * honest disclosure that the microphone is live.
 */
class CaptureService : Service() {

    companion object {
        private const val CHANNEL_ID = "talkee-capture"
        private const val NOTIFICATION_ID = 1

        /** Starts streaming, or does nothing if it is already running. */
        fun start(context: Context) {
            context.startForegroundService(Intent(context, CaptureService::class.java))
        }

        /** Stops streaming and removes the notification. */
        fun stop(context: Context) {
            context.stopService(Intent(context, CaptureService::class.java))
        }
    }

    /** Not a bound service; the plugin talks to [TalkeeEngine] directly. */
    override fun onBind(intent: Intent?): IBinder? = null

    private var cpuLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onCreate() {
        super.onCreate()
        ensureChannel()
        startForegroundCompat()
        holdLocks()
        TalkeeEngine.start(applicationContext)
    }

    /**
     * Keeps the processor and the Wi-Fi radio awake while streaming.
     *
     * A foreground service keeps the process alive; it does not stop the phone from napping the CPU
     * between audio buffers or putting the Wi-Fi radio into power save when the screen turns off. Both
     * show up as the same thing from the PC: audio that stutters or stops a minute after the phone goes
     * in a pocket. Held only while the service runs, and always released.
     */
    private fun holdLocks() {
        runCatching {
            val power = getSystemService(Context.POWER_SERVICE) as PowerManager
            cpuLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "talkee:capture").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
        runCatching {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            @Suppress("DEPRECATION")
            val mode = WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifiLock = wifi.createWifiLock(mode, "talkee:stream").apply {
                setReferenceCounted(false)
                acquire()
            }
        }
    }

    private fun releaseLocks() {
        runCatching { cpuLock?.takeIf { it.isHeld }?.release() }
        runCatching { wifiLock?.takeIf { it.isHeld }?.release() }
        cpuLock = null
        wifiLock = null
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // If the system reclaims us under memory pressure it should bring us back
        // unprompted — the user should never have to notice.
        return START_STICKY
    }

    override fun onDestroy() {
        TalkeeEngine.stop()
        releaseLocks()
        super.onDestroy()
    }

    private fun startForegroundCompat() {
        val notification = buildNotification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            // Declaring the type is what tells the platform this is a legitimate
            // microphone use rather than a service trying to hide.
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    private fun buildNotification(): Notification {
        val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CHANNEL_ID)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }
        return builder
            .setContentTitle("Talkee")
            .setContentText("Your phone is acting as a microphone")
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setOngoing(true)
            .setShowWhen(false)
            .build()
    }

    private fun ensureChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Microphone sharing",
                // LOW: this is a status indicator, not something to alert about.
                NotificationManager.IMPORTANCE_LOW,
            )
            channel.description = "Shown while your phone is acting as a microphone"
            channel.setShowBadge(false)
            manager.createNotificationChannel(channel)
        }
    }
}
