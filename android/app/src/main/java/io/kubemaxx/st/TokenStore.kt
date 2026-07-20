package io.kubemaxx.st

import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.nio.ByteBuffer
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

internal class TokenStore(private val preferences: SharedPreferences) {
    fun loadAndMigrate(): String {
        preferences.getString(ENCRYPTED_TOKEN, null)?.let { encoded ->
            return decrypt(encoded).getOrElse {
                preferences.edit().remove(ENCRYPTED_TOKEN).apply()
                ""
            }
        }

        val plaintext = preferences.getString(PLAINTEXT_TOKEN, "")?.trim().orEmpty()
        if (plaintext.isNotEmpty()) {
            save(plaintext)
        } else {
            preferences.edit().remove(PLAINTEXT_TOKEN).apply()
        }
        return plaintext
    }

    fun save(token: String): Boolean {
        if (token.isEmpty()) {
            return preferences.edit()
                .remove(ENCRYPTED_TOKEN)
                .remove(PLAINTEXT_TOKEN)
                .commit()
        }

        val encrypted = runCatching {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, secretKey())
            TokenCiphertext.encode(cipher.iv, cipher.doFinal(token.toByteArray(Charsets.UTF_8)))
        }.getOrNull()
        if (encrypted == null) {
            preferences.edit().remove(PLAINTEXT_TOKEN).apply()
            return false
        }
        return preferences.edit()
            .putString(ENCRYPTED_TOKEN, Base64.encodeToString(encrypted, Base64.NO_WRAP))
            .remove(PLAINTEXT_TOKEN)
            .commit()
    }

    private fun decrypt(encoded: String): Result<String> = runCatching {
        val envelope = requireNotNull(
            TokenCiphertext.decode(Base64.decode(encoded, Base64.NO_WRAP)),
        )
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, secretKey(), GCMParameterSpec(GCM_TAG_BITS, envelope.iv))
        cipher.doFinal(envelope.ciphertext).toString(Charsets.UTF_8).trim()
    }

    private fun secretKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE).run {
            init(
                KeyGenParameterSpec.Builder(
                    KEY_ALIAS,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setRandomizedEncryptionRequired(true)
                    .build(),
            )
            generateKey()
        }
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val KEY_ALIAS = "st.authentication-token.v1"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val GCM_TAG_BITS = 128
        const val ENCRYPTED_TOKEN = "token_encrypted_v1"
        const val PLAINTEXT_TOKEN = "token"
    }
}

internal data class TokenCiphertext(val iv: ByteArray, val ciphertext: ByteArray) {
    companion object {
        fun encode(iv: ByteArray, ciphertext: ByteArray): ByteArray {
            require(iv.isNotEmpty() && iv.size <= UByte.MAX_VALUE.toInt())
            require(ciphertext.size >= GCM_TAG_BYTES)
            return ByteBuffer.allocate(2 + iv.size + ciphertext.size)
                .put(VERSION)
                .put(iv.size.toByte())
                .put(iv)
                .put(ciphertext)
                .array()
        }

        fun decode(encoded: ByteArray): TokenCiphertext? = runCatching {
            val buffer = ByteBuffer.wrap(encoded)
            require(buffer.remaining() >= 2)
            require(buffer.get() == VERSION)
            val ivLength = buffer.get().toInt() and 0xff
            require(ivLength > 0 && buffer.remaining() >= ivLength + GCM_TAG_BYTES)
            val iv = ByteArray(ivLength)
            buffer.get(iv)
            val ciphertext = ByteArray(buffer.remaining())
            buffer.get(ciphertext)
            TokenCiphertext(iv, ciphertext)
        }.getOrNull()

        private const val VERSION: Byte = 1
        private const val GCM_TAG_BYTES = 16
    }
}
