package network.tollgate.android.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log
import okhttp3.*
import okhttp3.WebSocket
import okio.ByteString
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/**
 * Foreground service that provides FIPS notifications WITHOUT Firebase Cloud
 * Messaging (FCM) or Apple Push Notification Service (APNS).
 *
 * It maintains a persistent WebSocket subscription to the user's Nostr relays.
 * When a relevant event arrives — a Cashu payment (NIP-60 kind 7375/7376),
 * a direct message (NIP-17/1059), or a notification (NIP-04 kind 4) — it fires
 * a local Android notification.
 *
 * This is the "no FCM" push path: the app holds its own relay connection and
 * surfaces events immediately, even when the app is backgrounded, because the
 * foreground service keeps the process alive.
 *
 * Lifecycle:
 *   startForeground() → connect to relay → subscribe → onMessage → notify
 *   onDestroy → close WebSocket
 */
class NostrListenerService : Service() {

    private val client = OkHttpClient.Builder()
        .pingInterval(30, TimeUnit.SECONDS)
        .build()

    private val webSockets = mutableListOf<WebSocket>()
    private val notificationManager by lazy {
        getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
    }

    // Relays to listen on. In production these come from the wallet's config.
    private val relays = listOf(
        "wss://relay.damus.io",
        "wss://nos.lol",
        "wss://relay.primal.net",
    )

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(NOTIF_ID_FG, buildForegroundNotification())
        connectToRelays()
        return START_STICKY  // restart if killed
    }

    override fun onDestroy() {
        super.onDestroy()
        webSockets.forEach { it.close(1000, "service stopping") }
        webSockets.clear()
        Log.i(TAG, "Service destroyed, WebSockets closed")
    }

    // --- WebSocket relay connection ---

    private fun connectToRelays() {
        for (relayUrl in relays) {
            val req = Request.Builder().url(relayUrl).build()
            val ws = client.newWebSocket(req, object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    Log.i(TAG, "Connected to $relayUrl")
                    // NIP-01 subscription for notifications:
                    //   kinds 4 (DM), 1059 (gift-wrapped DM), 7375/7376 (Cashu wallet)
                    // We subscribe broadly; the npub filter would narrow this.
                    val subId = "tg_${System.currentTimeMillis()}"
                    val filterJson = JSONObject()
                        .put("kinds", JSONArray(listOf(4, 1059, 7375, 7376)))
                        .put("limit", 0)  // only new events, no backlog
                    val reqMsg = JSONArray()
                        .put("REQ")
                        .put(subId)
                        .put(filterJson)
                        .toString()
                    webSocket.send(reqMsg)
                    Log.i(TAG, "Subscribed on $relayUrl (sub=$subId)")
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    handleRelayMessage(text, relayUrl)
                }

                override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                    handleRelayMessage(bytes.utf8(), relayUrl)
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    Log.e(TAG, "Relay $relayUrl failed: ${t.message}")
                    // OkHttp ping interval handles keepalive; START_STICKY handles
                    // process death. For transient failures, the next app launch
                    // or system restart will reconnect.
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    Log.w(TAG, "Relay $relayUrl closed: $code $reason")
                }
            })
            webSockets.add(ws)
        }
    }

    /**
     * Parse a NIP-01 relay message. We only care about EVENT messages —
     * EOSE (end of stored events) and NOTICE are logged but ignored.
     */
    private fun handleRelayMessage(raw: String, relayUrl: String) {
        try {
            val arr = JSONArray(raw)
            val type = arr.optString(0)
            when (type) {
                "EVENT" -> {
                    val event = arr.optJSONObject(2) ?: return
                    val kind = event.optInt("kind", -1)
                    val pubkey = event.optString("pubkey", "")
                    val content = event.optString("content", "")
                    Log.d(TAG, "Event kind=$kind from ${pubkey.take(12)}…")
                    notifyEvent(kind, pubkey, content)
                }
                "EOSE" -> Log.d(TAG, "EOSE on $relayUrl")
                "NOTICE" -> Log.d(TAG, "Notice from $relayUrl: ${arr.optString(1)}")
            }
        } catch (e: Exception) {
            Log.w(TAG, "Bad relay message from $relayUrl: ${e.message}")
        }
    }

    /**
     * Fire a local notification for an incoming Nostr event.
     * Encrypted events (NIP-04/NIP-44) will show as "Encrypted message"
     * since decryption happens in the Rust core.
     */
    private fun notifyEvent(kind: Int, pubkey: String, content: String) {
        val (title, body) = when (kind) {
            7375, 7376 -> "Cashu wallet update" to "Your NIP-60 wallet changed"
            1059 -> "Direct message" to "You received a gift-wrapped message"
            4 -> "Direct message" to "You received an encrypted message"
            else -> "Nostr event" to "Kind $kind from ${pubkey.take(16)}…"
        }

        val notif = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(body)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setAutoCancel(true)
            .setVibrate(longArrayOf(0, 200, 100, 200))
            .build()

        notificationManager.notify(NOTIF_ID_BASE + kind, notif)
    }

    // --- Notifications ---

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                "TollGate Notifications",
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply {
                description = "Nostr relay notifications (no FCM)"
                enableVibration(true)
            }
            notificationManager.createNotificationChannel(channel)
        }
    }

    private fun buildForegroundNotification(): Notification {
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("TollGate Active")
            .setContentText("Listening for Nostr events (${relays.size} relays)")
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setOngoing(true)
            .build()
    }

    companion object {
        private const val TAG = "NostrListener"
        private const val CHANNEL_ID = "tollgate_notifications"
        private const val NOTIF_ID_FG = 1
        private const val NOTIF_ID_BASE = 100

        fun start(context: Context) {
            val intent = Intent(context, NostrListenerService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, NostrListenerService::class.java))
        }
    }
}
