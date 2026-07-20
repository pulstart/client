package io.kubemaxx.st

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectionStatusTest {
    @Test
    fun directAndApiConnectingStatusesAreNonTerminal() {
        assertTrue(isConnectionPending("connecting"))
        assertTrue(isConnectionPending("connecting through API"))
        assertFalse(isConnectionPending("connected"))
        assertFalse(isConnectionPending("error: connection failed"))
        assertFalse(isConnectionPending("disconnected"))
    }
}
