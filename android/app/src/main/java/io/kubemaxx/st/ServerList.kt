package io.kubemaxx.st

import java.util.Locale

internal data class SavedServer(
    val address: String,
    val nickname: String = "",
    val peerId: String? = null,
    val lastConnected: Long = 0,
    val manuallyAdded: Boolean = true,
    val legacyToken: String? = null,
)

internal data class LanDiscoveredServer(
    val hostname: String,
    val address: String,
    val peerId: String,
)

internal enum class ServerPath(val label: String) {
    LAN("LAN"),
    VPN("VPN"),
    WAN("WAN"),
}

internal data class ServerCard(
    val key: String,
    val displayName: String,
    val subtitle: String,
    val connectAddress: String?,
    val tunnelPeerId: String?,
    val savedAddress: String?,
    val peerId: String?,
    val path: ServerPath?,
    val online: Boolean,
    val apiOnly: Boolean,
)

internal fun normalizeServerAddress(input: String): String? {
    val value = input.trim()
    if (value.isEmpty()) return null
    if (value.startsWith('[') || value.count { it == ':' } > 1) return null
    val separator = value.lastIndexOf(':')
    if (separator >= 0) {
        val host = value.substring(0, separator)
        val port = value.substring(separator + 1).toIntOrNull()
        return value.takeIf { host.isNotEmpty() && port in 1..65535 }
    }
    return "$value:$DEFAULT_SERVER_PORT"
}

internal fun reconcileSavedServers(
    saved: List<SavedServer>,
    lan: List<LanDiscoveredServer>,
): List<SavedServer> = saved.map { entry ->
    if (entry.peerId != null) return@map entry
    val live = lan.firstOrNull { it.address == entry.address } ?: return@map entry
    entry.copy(
        nickname = entry.nickname.ifEmpty { live.hostname },
        peerId = entry.peerId ?: live.peerId,
    )
}

internal fun buildServerCards(
    saved: List<SavedServer>,
    lan: List<LanDiscoveredServer>,
    apiHost: ApiHostPresence?,
): List<ServerCard> {
    val validLan = lan.filter { it.peerId.isNotEmpty() }
    val lanByPeer = validLan.associateBy(LanDiscoveredServer::peerId)
    val apiPeer = apiHost?.peerId?.takeIf(String::isNotEmpty)
    val usedPeers = mutableSetOf<String>()
    var apiUsed = false
    val cards = mutableListOf<ServerCard>()

    saved.filter(SavedServer::manuallyAdded)
        .sortedByDescending(SavedServer::lastConnected)
        .distinctBy { it.peerId ?: it.address }
        .forEach { entry ->
            val live = entry.peerId?.let(lanByPeer::get)
                ?: validLan.firstOrNull { entry.peerId == null && it.address == entry.address }
            val apiOnline = apiPeer != null && apiPeer == (entry.peerId ?: live?.peerId)
            val addressConflict = entry.peerId != null && validLan.any {
                it.address == entry.address && it.peerId != entry.peerId
            }
            live?.peerId?.let(usedPeers::add)
            apiUsed = apiUsed || apiOnline
            val address = live?.address ?: entry.address
            val online = live != null || apiOnline
            val useTunnel = live == null && apiOnline
            cards += ServerCard(
                key = entry.peerId ?: entry.address,
                displayName = entry.nickname.ifEmpty {
                    live?.hostname ?: apiHost?.hostname?.takeIf { apiOnline } ?: entry.address
                },
                subtitle = when {
                    live != null -> live.address
                    apiOnline -> "Online through API - encrypted tunnel"
                    addressConflict -> "Saved address belongs to another computer"
                    entry.nickname.isNotEmpty() -> entry.address
                    else -> formatLastConnected(entry.lastConnected)
                },
                connectAddress = address.takeUnless { useTunnel || addressConflict },
                tunnelPeerId = apiPeer.takeIf { apiOnline },
                savedAddress = entry.address,
                peerId = entry.peerId ?: live?.peerId,
                path = live?.let { classifyServerPath(address) }
                    ?: ServerPath.WAN.takeIf { useTunnel },
                online = online,
                apiOnly = useTunnel,
            )
        }

    validLan.forEach { server ->
        if (!usedPeers.add(server.peerId)) return@forEach
        val apiOnline = apiPeer != null && apiPeer == server.peerId
        apiUsed = apiUsed || apiOnline
        cards += ServerCard(
            key = server.peerId,
            displayName = server.hostname.ifEmpty { server.address },
            subtitle = server.address,
            connectAddress = server.address,
            tunnelPeerId = apiPeer.takeIf { apiOnline },
            savedAddress = null,
            peerId = server.peerId,
            path = classifyServerPath(server.address),
            online = true,
            apiOnly = false,
        )
    }

    if (apiHost != null && apiPeer != null && !apiUsed) {
        val hostname = apiHost.hostname ?: "Host"
        cards += ServerCard(
            key = apiPeer ?: "api:${hostname.lowercase(Locale.ROOT)}",
            displayName = hostname,
            subtitle = "Online through API - tunnel required",
            connectAddress = null,
            tunnelPeerId = apiPeer,
            savedAddress = null,
            peerId = apiPeer,
            path = null,
            online = true,
            apiOnly = true,
        )
    }
    return cards
}

internal fun classifyServerPath(address: String): ServerPath {
    val host = addressHost(address)
    val octets = host.split('.').mapNotNull(String::toIntOrNull)
    if (octets.size != 4 || octets.any { it !in 0..255 }) return ServerPath.WAN
    return when {
        octets[0] == 10 -> ServerPath.VPN
        octets[0] == 100 && octets[1] in 64..127 -> ServerPath.VPN
        octets[0] == 127 || octets[0] == 0 -> ServerPath.LAN
        octets[0] == 169 && octets[1] == 254 -> ServerPath.LAN
        octets[0] == 192 && octets[1] == 168 -> ServerPath.LAN
        octets[0] == 172 && octets[1] in 16..31 -> ServerPath.LAN
        else -> ServerPath.WAN
    }
}

internal fun formatLastConnected(timestampSeconds: Long, nowSeconds: Long = System.currentTimeMillis() / 1_000): String {
    if (timestampSeconds <= 0) return "Never connected"
    val age = (nowSeconds - timestampSeconds).coerceAtLeast(0)
    return when {
        age < 60 -> "Just now"
        age < 3_600 -> "${age / 60} min ago"
        age < 86_400 -> "${age / 3_600} hours ago"
        age < 86_400 * 30 -> "${age / 86_400} days ago"
        else -> "Long ago"
    }
}

private fun addressHost(address: String): String {
    if (address.startsWith('[')) return address.substringAfter('[').substringBefore(']')
    return address.substringBeforeLast(':', address)
}

private const val DEFAULT_SERVER_PORT = 28_480
