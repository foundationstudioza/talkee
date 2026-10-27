package net.talkee.talkee_app

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import java.net.InetAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Finds a PC this phone is already paired with, after its address has changed.
 *
 * # Why this exists
 *
 * The pairing code carries the PC's address, and routers hand addresses out afresh: a PC that was
 * 192.168.0.28 yesterday can be .31 today. Without this the phone would knock on the old address
 * for ever and the only cure would be scanning again. The PC advertises its key's fingerprint over
 * mDNS, so a phone can look for *its* PC by that label.
 *
 * This only finds an address. It never trusts what it finds: the phone still has to prove it holds
 * the pairing key before the PC accepts anything, so a stranger advertising the same label gains
 * nothing.
 */
object HostFinder {

    private const val TXT_FINGERPRINT = "fp"

    /** The address of the PC whose pairing fingerprint is [fingerprint], or null if none answered. */
    fun find(context: Context, fingerprint: String, timeoutMs: Long = 6_000): InetAddress? {
        if (fingerprint.isBlank()) return null
        val nsd = context.getSystemService(Context.NSD_SERVICE) as? NsdManager ?: return null

        val done = CountDownLatch(1)
        val found = java.util.concurrent.atomic.AtomicReference<InetAddress?>(null)
        val queue = LinkedBlockingQueue<NsdServiceInfo>()
        // NsdManager allows one resolve at a time and fails the second with FAILURE_ALREADY_ACTIVE,
        // so found services are queued and resolved one by one.
        val resolving = AtomicBoolean(false)

        fun resolveNext() {
            if (done.count == 0L || !resolving.compareAndSet(false, true)) return
            val next = queue.poll()
            if (next == null) {
                resolving.set(false)
                return
            }
            try {
                nsd.resolveService(next, object : NsdManager.ResolveListener {
                    override fun onResolveFailed(serviceInfo: NsdServiceInfo, errorCode: Int) {
                        resolving.set(false)
                        resolveNext()
                    }

                    override fun onServiceResolved(serviceInfo: NsdServiceInfo) {
                        val label = serviceInfo.attributes?.get(TXT_FINGERPRINT)?.toString(Charsets.UTF_8)
                        if (label == fingerprint && serviceInfo.host != null) {
                            found.set(serviceInfo.host)
                            done.countDown()
                        }
                        resolving.set(false)
                        resolveNext()
                    }
                })
            } catch (_: Exception) {
                resolving.set(false)
            }
        }

        val listener = object : NsdManager.DiscoveryListener {
            override fun onDiscoveryStarted(serviceType: String) = Unit
            override fun onServiceFound(service: NsdServiceInfo) {
                queue.add(service)
                resolveNext()
            }
            override fun onServiceLost(service: NsdServiceInfo) = Unit
            override fun onDiscoveryStopped(serviceType: String) = Unit
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                done.countDown()
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) = Unit
        }

        try {
            nsd.discoverServices(TalkeeEngine.SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
            done.await(timeoutMs, TimeUnit.MILLISECONDS)
        } catch (_: InterruptedException) {
            // The engine was stopped while looking.
        } catch (_: Exception) {
            // No discovery on this network. Not worth more than "not found".
        } finally {
            runCatching { nsd.stopServiceDiscovery(listener) }
        }
        return found.get()
    }
}
