package net.talkee.talkee_app

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Remembers the pairing link between runs, encrypted with a key only this app can use.
 *
 * # Why this exists
 *
 * Without it, every launch would need the code scanned again — which turns a one-time setup into a
 * daily chore, and a daily chore is one people stop doing carefully.
 *
 * # Why the Keystore rather than a preference
 *
 * The link *contains the pairing key*. Whoever reads it can speak through the user's microphone, so
 * storing it in plain text would hand that to anything that can read the app's data — including a
 * backup, a rooted device, or a bug in another app on the same profile.
 * `docs/PROTOCOL.md` states the rule as *"stored in the platform keystore"*, and this is it: an AES
 * key generated inside the Android Keystore, which does not leave it, encrypting the link with GCM.
 *
 * # What it stores
 *
 * The link text, exactly as it arrived, so nothing about [PairLink] has to be serialised and no
 * field can be silently dropped by a round trip. It is re-parsed on the way out, by the same code
 * that parses a scan.
 */
object PairingStore {

    private const val PREFS = "talkee"
    private const val ENTRY = "pairing"
    private const val ALIAS = "talkee-pairing"
    private const val TRANSFORM = "AES/GCM/NoPadding"

    /** GCM tag length. 128 bits is the maximum and there is no reason to want less. */
    private const val TAG_BITS = 128

    /**
     * Bytes of initialisation vector.
     *
     * The Keystore generates a 12-byte GCM IV, which is fixed by the platform rather than chosen
     * here — this constant is the split point for `iv || ciphertext`, and a wrong split would make
     * the stored value undecryptable. That failure is survivable ([load] forgets and re-pairs)
     * rather than silent, which is why the format does not need to be self-describing.
     */
    private const val IV_BYTES = 12

    /** Stores the link, returning false when it could not be encrypted or written. */
    fun save(context: Context, uri: String): Boolean {
        val sealed = try {
            encrypt(uri)
        } catch (_: Exception) {
            // Report rather than swallow: a save that quietly does nothing means the user re-scans
            // the code on every launch and never learns why.
            return false
        }
        return prefs(context).edit().putString(ENTRY, sealed).commit()
    }

    /** The stored link, or null when there is none usable. */
    fun load(context: Context): String? {
        val stored = prefs(context).getString(ENTRY, null) ?: return null
        return try {
            decrypt(stored)
        } catch (_: Exception) {
            // A Keystore key can be invalidated without warning — a device credential change, a
            // restore onto different hardware, an OS upgrade. The stored link then cannot be read,
            // and forgetting it is the only honest answer: the user pairs again instead of owning an
            // app that cannot explain itself.
            clear(context)
            null
        }
    }

    /** Forgets the pairing, so the next launch scans again. */
    fun clear(context: Context) {
        prefs(context).edit().remove(ENTRY).apply()
    }

    private fun prefs(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The app's encryption key, created on first use and never exported. */
    private fun key(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
        return generator.generateKey()
    }

    private fun encrypt(plain: String): String {
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val body = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(cipher.iv + body, Base64.NO_WRAP)
    }

    private fun decrypt(stored: String): String {
        val all = Base64.decode(stored, Base64.NO_WRAP)
        val iv = all.copyOfRange(0, IV_BYTES)
        val body = all.copyOfRange(IV_BYTES, all.size)
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, iv))
        return String(cipher.doFinal(body), Charsets.UTF_8)
    }
}
