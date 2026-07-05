package network.tollgate.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AccountBalanceWallet
import androidx.compose.material.icons.filled.BugReport
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.lifecycle.viewmodel.compose.viewModel
import network.tollgate.android.ui.WalletViewModel
import network.tollgate.android.ui.screens.DiagnosticsScreen
import network.tollgate.android.ui.screens.WalletScreen
import network.tollgate.android.ui.theme.TollGateTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            TollGateTheme {
                TollGateApp()
            }
        }
    }
}

@Composable
fun TollGateApp(viewModel: WalletViewModel = viewModel()) {
    val state by viewModel.uiState.collectAsState()
    var currentScreen by remember { mutableStateOf("wallet") }

    Scaffold(
        bottomBar = {
            NavigationBar {
                NavigationBarItem(
                    selected = currentScreen == "wallet",
                    onClick = { currentScreen = "wallet" },
                    icon = { Icon(Icons.Default.AccountBalanceWallet, contentDescription = null) },
                    label = { Text("Wallet") }
                )
                NavigationBarItem(
                    selected = currentScreen == "diagnostics",
                    onClick = { currentScreen = "diagnostics" },
                    icon = { Icon(Icons.Default.BugReport, contentDescription = null) },
                    label = { Text("Diagnostics") }
                )
            }
        }
    ) { padding ->
        when (currentScreen) {
            "wallet" -> WalletScreen(
                state = state,
                onCreateWallet = viewModel::createWallet,
                onRestoreWallet = viewModel::restoreWallet,
                onSync = viewModel::syncWallet,
                onDeposit = viewModel::depositToken,
                onVerify = viewModel::verifyToken,
            )
            "diagnostics" -> DiagnosticsScreen(state)
        }
    }
}
