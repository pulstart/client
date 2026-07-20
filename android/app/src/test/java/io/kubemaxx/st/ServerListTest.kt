package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ServerListTest {
    @Test
    fun normalizesDesktopStyleServerAddresses() {
        assertEquals("stream-box:28480", normalizeServerAddress(" stream-box "))
        assertEquals("stream-box:1234", normalizeServerAddress("stream-box:1234"))
        assertNull(normalizeServerAddress("2001:db8::1"))
        assertNull(normalizeServerAddress("[2001:db8::1]"))
        assertNull(normalizeServerAddress("stream-box:70000"))
    }

    @Test
    fun reconcilesSavedIdentityFromMatchingLanBeacon() {
        val reconciled = reconcileSavedServers(
            listOf(SavedServer(address = "192.168.1.2:28480")),
            listOf(LanDiscoveredServer("Desktop", "192.168.1.2:28480", "peer-a")),
        ).single()

        assertEquals("Desktop", reconciled.nickname)
        assertEquals("peer-a", reconciled.peerId)
    }

    @Test
    fun doesNotReplaceStableIdentityWhenAddressIsReused() {
        val saved = SavedServer(
            address = "192.168.1.2:28480",
            nickname = "Old PC",
            peerId = "peer-old",
        )
        val lan = LanDiscoveredServer("New PC", "192.168.1.2:28480", "peer-new")

        assertEquals(saved, reconcileSavedServers(listOf(saved), listOf(lan)).single())
        val cards = buildServerCards(listOf(saved), listOf(lan), null)
        assertEquals(2, cards.size)
        val oldCard = cards.first { it.peerId == "peer-old" }
        assertFalse(oldCard.online)
        assertNull(oldCard.connectAddress)
        assertTrue(cards.first { it.peerId == "peer-new" }.online)
    }

    @Test
    fun mergesSavedLanAndApiVariantsIntoOneCard() {
        val cards = buildServerCards(
            saved = listOf(
                SavedServer(
                    address = "203.0.113.2:28480",
                    nickname = "Gaming PC",
                    peerId = "peer-a",
                ),
            ),
            lan = listOf(LanDiscoveredServer("Desktop", "192.168.1.2:28480", "peer-a")),
            apiHost = ApiHostPresence("Desktop", "peer-a"),
        )

        assertEquals(1, cards.size)
        assertEquals("Gaming PC", cards.single().displayName)
        assertEquals("192.168.1.2:28480", cards.single().connectAddress)
        assertEquals("peer-a", cards.single().tunnelPeerId)
        assertEquals(ServerPath.LAN, cards.single().path)
        assertTrue(cards.single().online)
        assertFalse(cards.single().apiOnly)
    }

    @Test
    fun apiOnlyPresenceUsesTunnelTarget() {
        val card = buildServerCards(
            saved = emptyList(),
            lan = emptyList(),
            apiHost = ApiHostPresence("Remote PC", "peer-b"),
        ).single()

        assertEquals("Remote PC", card.displayName)
        assertNull(card.connectAddress)
        assertEquals("peer-b", card.tunnelPeerId)
        assertTrue(card.online)
        assertTrue(card.apiOnly)
    }

    @Test
    fun dropsApiPresenceWithoutStableIdentity() {
        assertTrue(
            buildServerCards(
                saved = emptyList(),
                lan = emptyList(),
                apiHost = ApiHostPresence("Remote PC", null),
            ).isEmpty(),
        )
    }

    @Test
    fun unsavedLanServerKeepsMatchingApiFallback() {
        val card = buildServerCards(
            saved = emptyList(),
            lan = listOf(LanDiscoveredServer("Desktop", "192.168.1.2:28480", "peer-a")),
            apiHost = ApiHostPresence("Desktop", "peer-a"),
        ).single()

        assertEquals("192.168.1.2:28480", card.connectAddress)
        assertEquals("peer-a", card.tunnelPeerId)
        assertFalse(card.apiOnly)
    }

    @Test
    fun apiPresenceDoesNotClaimSavedDirectAddressIsReachable() {
        val card = buildServerCards(
            saved = listOf(
                SavedServer(address = "192.168.1.2:28480", peerId = "peer-b"),
            ),
            lan = emptyList(),
            apiHost = ApiHostPresence("Remote PC", "peer-b"),
        ).single()

        assertTrue(card.online)
        assertNull(card.connectAddress)
        assertEquals("peer-b", card.tunnelPeerId)
        assertEquals(ServerPath.WAN, card.path)
        assertEquals("Online through API - encrypted tunnel", card.subtitle)
    }

    @Test
    fun classifiesLocalVpnAndPublicPaths() {
        assertEquals(ServerPath.LAN, classifyServerPath("192.168.1.2:28480"))
        assertEquals(ServerPath.VPN, classifyServerPath("100.64.1.2:28480"))
        assertEquals(ServerPath.WAN, classifyServerPath("203.0.113.2:28480"))
    }
}
