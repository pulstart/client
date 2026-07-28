package io.kubemaxx.st

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class UpdateCheckTest {
    @Test
    fun parsesTagAndUrlFromReleaseResponse() {
        val release = UpdateCheck.parseLatestRelease(
            """
            {
              "tag_name": "v0.12.8",
              "html_url": "https://github.com/pulstart/client/releases/tag/v0.12.8",
              "assets": [{"name": "st-client-v0.12.8-android.apk"}]
            }
            """.trimIndent(),
        )
        assertEquals("0.12.8", release.version)
        assertEquals("https://github.com/pulstart/client/releases/tag/v0.12.8", release.htmlUrl)
    }

    @Test
    fun missingHtmlUrlFallsBackToReleasesPage() {
        val release = UpdateCheck.parseLatestRelease("""{"tag_name": "v1.0.0"}""")
        assertEquals(UpdateCheck.RELEASES_PAGE, release.htmlUrl)
    }

    /** A string compare would call 0.12.10 older than 0.12.8. */
    @Test
    fun comparesComponentsNumericallyNotLexically() {
        assertTrue(UpdateCheck.isNewer("0.12.10", "0.12.8"))
        assertFalse(UpdateCheck.isNewer("0.12.8", "0.12.10"))
    }

    @Test
    fun sameVersionIsNotNewer() {
        assertFalse(UpdateCheck.isNewer("0.12.8", "0.12.8"))
    }

    @Test
    fun handlesVPrefixOnEitherSide() {
        assertTrue(UpdateCheck.isNewer("v0.13.0", "0.12.8"))
        assertFalse(UpdateCheck.isNewer("v0.12.8", "v0.12.8"))
    }

    @Test
    fun shorterVersionTreatsMissingComponentsAsZero() {
        assertTrue(UpdateCheck.isNewer("0.13", "0.12.9"))
        assertFalse(UpdateCheck.isNewer("0.12", "0.12.0"))
    }

    /** A malformed tag must never prompt the user to "update" to garbage. */
    @Test
    fun malformedTagIsNotNewer() {
        assertFalse(UpdateCheck.isNewer("nightly", "0.12.8"))
        assertFalse(UpdateCheck.isNewer("", "0.12.8"))
    }

    @Test
    fun prereleaseSuffixIsIgnoredForComparison() {
        assertFalse(UpdateCheck.isNewer("0.12.8-rc1", "0.12.8"))
        assertTrue(UpdateCheck.isNewer("0.12.9-rc1", "0.12.8"))
    }
}
