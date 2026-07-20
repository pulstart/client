package io.kubemaxx.st

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNull
import org.junit.Test

class TokenStoreTest {
    @Test
    fun ciphertextEnvelopeRoundTripsWithoutPlaintextMetadata() {
        val iv = ByteArray(12) { it.toByte() }
        val ciphertext = ByteArray(32) { (it + 32).toByte() }

        val decoded = requireNotNull(TokenCiphertext.decode(TokenCiphertext.encode(iv, ciphertext)))

        assertArrayEquals(iv, decoded.iv)
        assertArrayEquals(ciphertext, decoded.ciphertext)
    }

    @Test
    fun ciphertextEnvelopeRejectsTruncationAndUnknownVersion() {
        assertNull(TokenCiphertext.decode(byteArrayOf()))
        assertNull(TokenCiphertext.decode(byteArrayOf(2, 12)))
        assertNull(TokenCiphertext.decode(byteArrayOf(1, 12, 1, 2, 3)))
    }
}
