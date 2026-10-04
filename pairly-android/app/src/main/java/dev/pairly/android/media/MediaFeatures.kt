package dev.pairly.android.media

import android.content.Context
import dev.pairly.core.ffi.MediaActionData
import dev.pairly.core.ffi.MediaHandler
import dev.pairly.core.ffi.PlayerData

/** Media control for the Rust core. Called on Rust threads. */
class MediaFeatures(context: Context) : MediaHandler {
    private val context = context.applicationContext

    override fun players(): List<PlayerData> = PhoneMedia.players()

    override fun artwork(key: String): ByteArray? = PhoneMedia.artwork(key)

    override fun peerPlayers(fromId: String, fromName: String, players: List<PlayerData>) =
        PcPlayers.show(context, fromId, fromName, players)

    override fun peerArtwork(fromId: String, key: String, data: ByteArray) = PcPlayers.artwork(context, key, data)

    override fun command(player: String, action: MediaActionData) = PhoneMedia.command(player, action)
}
