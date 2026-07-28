package io.kubemaxx.st

import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

internal data class ReleaseInfo(
    val version: String,
    val htmlUrl: String,
)

internal sealed interface UpdateStatus {
    data class UpToDate(val current: String) : UpdateStatus
    data class Available(val release: ReleaseInfo) : UpdateStatus
    data class Failed(val message: String) : UpdateStatus
}

/**
 * Checks GitHub for a newer client release.
 *
 * This only reports that an update exists and links to it. It deliberately does
 * not download or install: release APKs are signed with an ephemeral debug key
 * that differs on every CI run, so Android would reject the install with a
 * signature mismatch against the copy already on the device. Downloading an APK
 * that cannot install is worse than sending the user to the releases page.
 * Wiring a real installer means adopting a stable release keystore first.
 */
internal object UpdateCheck {
    fun fetchLatest(currentVersion: String): UpdateStatus = try {
        val release = requestLatestRelease(currentVersion)
        if (isNewer(release.version, currentVersion)) {
            UpdateStatus.Available(release)
        } else {
            UpdateStatus.UpToDate(currentVersion)
        }
    } catch (error: IOException) {
        UpdateStatus.Failed(error.message ?: "Could not reach GitHub")
    } catch (error: RuntimeException) {
        // JSONException and friends: a reachable endpoint returning something
        // unexpected must not take the activity down.
        UpdateStatus.Failed(error.message ?: "Unexpected release response")
    }

    private fun requestLatestRelease(currentVersion: String): ReleaseInfo {
        val connection = URL(RELEASES_API).openConnection() as HttpURLConnection
        try {
            connection.requestMethod = "GET"
            connection.connectTimeout = TIMEOUT_MS
            connection.readTimeout = TIMEOUT_MS
            connection.useCaches = false
            // GitHub rejects API requests that omit a User-Agent.
            connection.setRequestProperty("User-Agent", "st-client-android/$currentVersion")
            connection.setRequestProperty("Accept", "application/vnd.github+json")

            return when (val status = connection.responseCode) {
                in 200..299 -> connection.inputStream.bufferedReader(Charsets.UTF_8).use {
                    parseLatestRelease(it.readText())
                }
                else -> throw IOException("GitHub release request failed with HTTP $status")
            }
        } finally {
            connection.disconnect()
        }
    }

    internal fun parseLatestRelease(response: String): ReleaseInfo {
        val value = JSONObject(response)
        val tag = value.optString("tag_name").ifBlank {
            throw IOException("Release response has no tag_name")
        }
        return ReleaseInfo(
            version = tag.removePrefix("v"),
            htmlUrl = value.optString("html_url").ifBlank { RELEASES_PAGE },
        )
    }

    /**
     * Dotted numeric comparison, so 0.12.10 correctly beats 0.12.8 — a plain
     * string compare would not. Missing or non-numeric components count as 0,
     * so a malformed tag can never masquerade as a newer version.
     */
    internal fun isNewer(latest: String, current: String): Boolean {
        val left = versionComponents(latest)
        val right = versionComponents(current)
        repeat(maxOf(left.size, right.size)) { index ->
            val l = left.getOrElse(index) { 0 }
            val r = right.getOrElse(index) { 0 }
            if (l != r) return l > r
        }
        return false
    }

    private fun versionComponents(version: String): List<Int> = version
        .removePrefix("v")
        .substringBefore('-')
        .split('.')
        .map { it.toIntOrNull() ?: 0 }

    const val RELEASES_PAGE = "https://github.com/pulstart/client/releases"
    private const val RELEASES_API = "https://api.github.com/repos/pulstart/client/releases/latest"
    private const val TIMEOUT_MS = 10_000
}
