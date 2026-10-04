package dev.pairly.android

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import dev.pairly.core.ffi.PairlyException
import dev.pairly.core.ffi.SecretStore
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Stores the device's identity secret encrypted with an AES-GCM key that never leaves the
 * Android Keystore. File format: `[version=1][12-byte IV][ciphertext + tag]`.
 */
class KeystoreSecretStore(context: Context) : SecretStore {
    private val file = AtomicFile(File(context.noBackupFilesDir, "identity.bin"))

    override fun load(): ByteArray? = guard {
        if (!file.baseFile.exists()) return@guard null
        val data = file.readFully()
        if (data.size < 1 + IV_LEN + TAG_BITS / 8 || data[0] != VERSION) {
            throw PairlyException.Failed("identity file is corrupt")
        }
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, data, 1, IV_LEN))
        cipher.doFinal(data, 1 + IV_LEN, data.size - 1 - IV_LEN)
    }

    override fun store(secret: ByteArray) = guard {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val sealed = cipher.doFinal(secret)
        val out = file.startWrite()
        try {
            out.write(byteArrayOf(VERSION))
            out.write(cipher.iv)
            out.write(sealed)
            file.finishWrite(out)
        } catch (e: Exception) {
            file.failWrite(out)
            throw e
        }
    }

    private fun key(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        (keyStore.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
            .apply { init(spec) }
            .generateKey()
    }

    /** Report failures to Rust as a typed error instead of an unexpected exception. */
    private inline fun <T> guard(block: () -> T): T = try {
        block()
    } catch (e: PairlyException) {
        throw e
    } catch (e: Exception) {
        throw PairlyException.Failed("identity storage: ${e.message ?: e.javaClass.simpleName}")
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val ALIAS = "pairly-identity"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val VERSION: Byte = 1
        const val IV_LEN = 12
        const val TAG_BITS = 128
    }
}
