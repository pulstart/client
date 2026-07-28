package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Test

class AppVersionTest {
    @Test
    fun versionShowsNameAndCode() {
        assertEquals("0.12.7 (12007)", formatAppVersion("0.12.7", 12007))
    }

    @Test
    fun untaggedBuildShowsCheckedInVersion() {
        assertEquals("0.1.0 (1)", formatAppVersion("0.1.0", 1))
    }
}
