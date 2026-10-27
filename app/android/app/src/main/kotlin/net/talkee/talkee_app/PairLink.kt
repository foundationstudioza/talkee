package net.talkee.talkee_app

import java.net.URLDecoder
import java.util.Base64

/**
 * The `talkee://pair?...` link a phone scans.
 *
 * Ten fields' worth of protocol in one string: where the host is, what it calls itself, which key
 * to prove knowledge of, and (later, once there is more than one host) which host this phone has
 * paired with before.
 *
 * # Why the link carries the address as well as the key
 *
 * Because discovery can be blocked. Guest Wi-Fi, corporate Wi-Fi with client isolation, a VPN, or a
 * phone and a PC on different subnets all stop mDNS, and without the address in the link a user in
 * that situation would have nothing to fall back on. One scan fixes pairing *and* discovery.
 *
 * # Why a bad scan is not an exception
 *
 * This is fed by a camera pointed at a screen. Half of what it sees will be somebody's coffee cup,
 * a Discord window, or a different QR code entirely. Returning `null` for anything that is not a
 * Talkee link is the difference between "that is not a Talkee code" and a crash on launch.
 */
data class PairLink(
    /** Address the host is reachable at, as the link states it. */
    val host: String,
    /** Port the control channel listens on. The audio port is derived from it. */
    val controlPort: Int,
    /** What the host calls itself, for the UI. */
    val name: String,
    /** Short digest of the key, so a host can be recognised again without the key. */
    val fingerprint: String,
    /** The 32-byte pairing key. This is the secret the whole scheme rests on. */
    val key: ByteArray,
) {
    /** The address the audio datagrams go to. */
    val audioPort: Int get() = controlPort + 1

    /** This link pointing at a different address, for a PC that has moved. Everything else stays. */
    fun withHost(newHost: String): PairLink = copy(host = newHost)

    /**
     * The `talkee://pair?...` text this link was read from, so it can be stored again.
     *
     * The key is written back as base64url with no padding, exactly as the PC wrote it, and the name
     * is percent-encoded; [parse] reads both straight back.
     */
    fun toUri(): String {
        val keyText = Base64.getUrlEncoder().withoutPadding().encodeToString(key)
        val nameText = java.net.URLEncoder.encode(name, "UTF-8").replace("+", "%20")
        return "talkee://pair?v=1&h=$host&p=$controlPort&fpid=$fingerprint&name=$nameText&k=$keyText"
    }

    companion object {
        /** Every link starts with this. */
        private const val PREFIX = "talkee://pair"

        /** The only link version this build understands. */
        private const val LINK_VERSION = 1

        /**
         * Parses a scan, returning `null` when it is not a Talkee pairing link.
         *
         * Strict about the fields that matter and forgiving about the ones that do not: a link
         * missing a host, a port or a key is useless and is refused, while a link missing a name or
         * a fingerprint still describes somewhere to connect and is accepted.
         */
        fun parse(text: String): PairLink? {
            val trimmed = text.trim()
            if (!trimmed.startsWith(PREFIX)) {
                return null
            }
            val query = trimmed.substringAfter('?', "")
            if (query.isEmpty()) {
                return null
            }

            val fields = mutableMapOf<String, String>()
            for (field in query.split('&')) {
                if (field.isEmpty()) {
                    continue
                }
                val name = field.substringBefore('=', "")
                if (name.isEmpty()) {
                    continue
                }
                fields[name] = field.substringAfter('=', "")
            }

            if (fields["v"]?.toIntOrNull() != LINK_VERSION) {
                return null
            }
            val host = fields["h"]?.takeIf { it.isNotBlank() } ?: return null
            val port = fields["p"]?.toIntOrNull() ?: return null
            if (port !in 1..65535) {
                return null
            }
            // The key is base64url, which contains no `%` and no `+`, so it is decoded as base64
            // and deliberately not percent-decoded: running it through a URL decoder could only
            // corrupt it, and a corrupted key is a pairing that fails for no visible reason.
            val key = decodeBase64Url(fields["k"] ?: return null) ?: return null
            if (key.size != Crypto.PSK_LEN) {
                return null
            }

            return PairLink(
                host = host,
                controlPort = port,
                name = percentDecode(fields["name"] ?: "").takeIf { it.isNotBlank() } ?: host,
                fingerprint = fields["fpid"] ?: "",
                key = key,
            )
        }

        /** Percent-decodes a value, keeping the raw text when it is malformed. */
        private fun percentDecode(value: String): String =
            try {
                URLDecoder.decode(value, "UTF-8")
            } catch (_: Exception) {
                value
            }

        /** Decodes base64url, accepting a missing padding character. */
        private fun decodeBase64Url(text: String): ByteArray? =
            try {
                Base64.getUrlDecoder().decode(text)
            } catch (_: IllegalArgumentException) {
                null
            }
    }

    /** Compares by value, except the key, which is compared by content rather than identity. */
    override fun equals(other: Any?): Boolean =
        this === other ||
            (other is PairLink &&
                host == other.host &&
                controlPort == other.controlPort &&
                name == other.name &&
                fingerprint == other.fingerprint &&
                key.contentEquals(other.key))

    override fun hashCode(): Int {
        var result = host.hashCode()
        result = 31 * result + controlPort
        result = 31 * result + name.hashCode()
        result = 31 * result + fingerprint.hashCode()
        result = 31 * result + key.contentHashCode()
        return result
    }
}
