package net.talkee.talkee_app

import android.content.Context
import android.os.Build
import java.security.SecureRandom

/**
 * What this phone remembers about the mesh: who it is, and the devices it has joined.
 *
 * The peers carry their pair keys, so they rest under the same Android Keystore key as the mic
 * pairing ([PairingStore]). The device id is not secret (the other side learns it on joining), so it
 * is a plain preference.
 */
object MeshStore {

    private const val ENTRY = "mesh-peers"
    private const val ID_PREF = "mesh-device-id"

    /** 32 random hex characters, made once per install. */
    fun deviceId(context: Context): String {
        val prefs = context.getSharedPreferences("talkee", Context.MODE_PRIVATE)
        prefs.getString(ID_PREF, null)?.takeIf { it.length == 32 && it.hexToBytes() != null }?.let { return it }
        val fresh = ByteArray(16).also { SecureRandom().nextBytes(it) }.toHex()
        prefs.edit().putString(ID_PREF, fresh).apply()
        return fresh
    }

    /** What other devices call this phone. */
    fun deviceName(): String {
        val model = (Build.MODEL ?: "").trim()
        val maker = (Build.MANUFACTURER ?: "").trim()
        val name = if (model.isEmpty()) "Android phone"
        else if (maker.isEmpty() || model.startsWith(maker, ignoreCase = true)) model
        else "$maker $model"
        return name.take(64)
    }

    @Synchronized
    fun peers(context: Context): List<MeshPeer> =
        PairingStore.loadEntry(context, ENTRY)?.let { MeshPeers.decode(it) } ?: emptyList()

    @Synchronized
    fun add(context: Context, peer: MeshPeer): Boolean =
        PairingStore.saveEntry(context, ENTRY, MeshPeers.encode(MeshPeers.upsert(peers(context), peer)))

    /** Stores a newly joined device and the devices it introduced. */
    @Synchronized
    fun addWithIntros(context: Context, peer: MeshPeer, intros: List<MeshPeer>): Boolean {
        val all = MeshPeers.apply(MeshPeers.upsert(peers(context), peer), intros, deviceId(context))
        return PairingStore.saveEntry(context, ENTRY, MeshPeers.encode(all))
    }

    /** Syncs with every reachable peer and stores what was learned. Blocks on the network. */
    fun sync(context: Context): Boolean {
        val before = peers(context)
        val after = MeshClient.sync(before, deviceId(context), deviceName())
        synchronized(this) {
            // Re-read, so a forget made meanwhile is not undone.
            val current = peers(context)
            val merged = after.filter { a -> current.any { it.id == a.id } || before.none { it.id == a.id } }
            return PairingStore.saveEntry(context, ENTRY, MeshPeers.encode(merged))
        }
    }

    @Synchronized
    fun forget(context: Context, id: String): Boolean =
        PairingStore.saveEntry(context, ENTRY, MeshPeers.encode(peers(context).filter { it.id != id }))
}
