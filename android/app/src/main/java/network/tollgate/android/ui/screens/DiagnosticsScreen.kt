package network.tollgate.android.ui.screens

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import network.tollgate.android.ui.TollGateUiState

/**
 * Diagnostics screen: shows the native Rust core version, compiled features,
 * and the liveness probe result. Useful for verifying the UniFFI bridge works.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun DiagnosticsScreen(state: TollGateUiState) {
    Scaffold(
        topBar = {
            TopAppBar(title = { Text("Diagnostics") })
        }
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(16.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            Card(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier.padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Text("Native Core", style = MaterialTheme.typography.headlineMedium)
                    Divider()
                    InfoRow("Version", state.coreVersion)
                    InfoRow("Features", state.coreFeatures.joinToString(", "))
                    InfoRow("Bridge", "UniFFI (proc-macro mode)")
                    InfoRow("Rust runtime", "tokio multi-thread")
                    InfoRow("Nostr SDK", "rust-nostr 0.40")
                    InfoRow("Cashu", "NIP-60 wallet sync")
                }
            }

            Card(modifier = Modifier.fillMaxWidth()) {
                Column(
                    modifier = Modifier.padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp)
                ) {
                    Text("FIPS Notifications", style = MaterialTheme.typography.headlineMedium)
                    Divider()
                    InfoRow("Transport", "Nostr relay WebSocket")
                    InfoRow("Push provider", "None (no FCM/APNS)")
                    InfoRow("Mechanism", "Foreground service + local notification")
                    InfoRow("Relay subscription", "Persistent, auto-reconnect")
                }
            }

            if (state.npub.isNotBlank()) {
                Card(modifier = Modifier.fillMaxWidth()) {
                    Column(
                        modifier = Modifier.padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        Text("Identity", style = MaterialTheme.typography.headlineMedium)
                        Divider()
                        Text("npub", fontWeight = FontWeight.Bold, fontSize = 13.sp)
                        Text(
                            state.npub,
                            fontFamily = FontFamily.Monospace,
                            fontSize = 12.sp,
                            modifier = Modifier.fillMaxWidth()
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween
    ) {
        Text(label, color = MaterialTheme.colorScheme.onSurface.copy(alpha = 0.6f))
        Text(value, fontWeight = FontWeight.Medium)
    }
}
