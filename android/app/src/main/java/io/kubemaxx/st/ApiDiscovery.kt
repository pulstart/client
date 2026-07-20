package io.kubemaxx.st

import org.json.JSONObject
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

internal data class ApiHostPresence(
    val hostname: String?,
    val peerId: String?,
)

internal object ApiDiscovery {
    fun findHost(token: String): ApiHostPresence? {
        require(token.isNotEmpty() && token.toByteArray(Charsets.UTF_8).size <= MAX_TOKEN_BYTES) {
            "API token must contain 1..=$MAX_TOKEN_BYTES bytes"
        }
        val body = JSONObject().put("token", token).toString().toByteArray(Charsets.UTF_8)
        val connection = URL("$BASE_URL/api/session").openConnection() as HttpURLConnection
        try {
            connection.requestMethod = "POST"
            connection.instanceFollowRedirects = false
            connection.connectTimeout = TIMEOUT_MS
            connection.readTimeout = TIMEOUT_MS
            connection.useCaches = false
            connection.doOutput = true
            connection.setRequestProperty("Content-Type", "application/json")
            connection.setFixedLengthStreamingMode(body.size)
            connection.outputStream.use { it.write(body) }

            return when (val status = connection.responseCode) {
                HttpURLConnection.HTTP_NOT_FOUND -> null
                in 200..299 -> connection.inputStream.bufferedReader(Charsets.UTF_8).use {
                    parseHostPresence(it.readText())
                }
                else -> throw IOException("API session request failed with HTTP $status")
            }
        } finally {
            connection.disconnect()
        }
    }

    internal fun parseHostPresence(response: String): ApiHostPresence? {
        val value = JSONObject(response)
        val host = value.optJSONObject("host") ?: return null
        return ApiHostPresence(
            hostname = host.optionalString("hostname"),
            peerId = host.optionalString("peer_id"),
        )
    }

    private fun JSONObject.optionalString(name: String): String? =
        (opt(name) as? String)?.trim()?.takeIf(String::isNotEmpty)

    val BASE_URL: String = BuildConfig.ST_API_URL.trimEnd('/')
    private const val TIMEOUT_MS = 5_000
    private const val MAX_TOKEN_BYTES = 256
}
