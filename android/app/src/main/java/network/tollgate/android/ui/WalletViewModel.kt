package network.tollgate.android.ui

import android.util.Log
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import uniffi.tollgate_android.*

/**
 * UI state for the TollGate app. Mirrors the Rust core's state through UniFFI.
 */
data class TollGateUiState(
    val isReady: Boolean = false,
    val npub: String = "",
    val nsec: String = "",
    val balanceSats: ULong = 0uL,
    val isSyncing: Boolean = false,
    val errorMessage: String? = null,
    val statusMessage: String = "Tap 'Create Wallet' to begin",
    val coreVersion: String = "",
    val coreFeatures: List<String> = emptyList(),
)

/**
 * ViewModel wrapping the native Rust core via UniFFI.
 *
 * All Rust calls happen on Dispatchers.IO; results flow back to the UI thread
 * through StateFlow. The Rust core holds its own tokio runtime internally, so
 * we just call its synchronous FFI methods from a background coroutine.
 */
class WalletViewModel : ViewModel() {

    private val _uiState = MutableStateFlow(TollGateUiState())
    val uiState: StateFlow<TollGateUiState> = _uiState.asStateFlow()

    // Default relays + mints for the wallet. These match the TollGate infrastructure.
    private val defaultRelays = listOf(
        "wss://relay.damus.io",
        "wss://nos.lol",
        "wss://relay.primal.net",
    )
    private val defaultMints = listOf(
        "https://mint.minibits.cash",
    )

    // The native Rust wallet. Created lazily — never null after init.
    private var wallet: Wallet? = null

    // Stored identity (in-memory; a production app would persist encrypted).
    private var currentKeypair: KeyPair? = null

    init {
        // Probe the Rust core on startup.
        _uiState.value = _uiState.value.copy(
            coreVersion = coreVersion(),
            coreFeatures = coreFeatures(),
            statusMessage = helloTollgate(),
        )
    }

    /**
     * Generate a new Nostr identity and initialise the wallet.
     */
    fun createWallet() {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val kp = generateKeypair()
                currentKeypair = kp
                initWallet(kp)
                _uiState.value = _uiState.value.copy(
                    isReady = true,
                    npub = kp.npub,
                    nsec = kp.nsec,
                    statusMessage = "Wallet created",
                    errorMessage = null,
                )
            } catch (e: TollgateException) {
                _uiState.value = _uiState.value.copy(
                    errorMessage = "Failed: ${e.detailString()}"
                )
            }
        }
    }

    /**
     * Restore a wallet from an nsec1… string.
     */
    fun restoreWallet(nsec: String) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val kp = keypairFromNsec(nsec.trim())
                currentKeypair = kp
                initWallet(kp)
                _uiState.value = _uiState.value.copy(
                    isReady = true,
                    npub = kp.npub,
                    nsec = kp.nsec,
                    statusMessage = "Wallet restored",
                    errorMessage = null,
                )
            } catch (e: TollgateException) {
                _uiState.value = _uiState.value.copy(
                    errorMessage = "Restore failed: ${e.detailString()}"
                )
            }
        }
    }

    /**
     * Sync the NIP-60 wallet from Nostr relays.
     */
    fun syncWallet() {
        val w = wallet ?: run {
            _uiState.value = _uiState.value.copy(errorMessage = "No wallet")
            return
        }
        viewModelScope.launch(Dispatchers.IO) {
            _uiState.value = _uiState.value.copy(isSyncing = true)
            try {
                val balance = w.connectAndSync()
                _uiState.value = _uiState.value.copy(
                    balanceSats = balance,
                    isSyncing = false,
                    statusMessage = "Synced",
                    errorMessage = null,
                )
            } catch (e: TollgateException) {
                _uiState.value = _uiState.value.copy(
                    isSyncing = false,
                    errorMessage = "Sync failed: ${e.detailString()}"
                )
            }
        }
    }

    /**
     * Deposit a Cashu token string into the NIP-60 wallet.
     */
    fun depositToken(tokenStr: String) {
        val w = wallet ?: run {
            _uiState.value = _uiState.value.copy(errorMessage = "No wallet")
            return
        }
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val newBalance = w.depositToken(tokenStr.trim())
                _uiState.value = _uiState.value.copy(
                    balanceSats = newBalance,
                    statusMessage = "Token deposited",
                    errorMessage = null,
                )
            } catch (e: TollgateException) {
                _uiState.value = _uiState.value.copy(
                    errorMessage = "Deposit failed: ${e.detailString()}"
                )
            }
        }
    }

    /**
     * Verify a Cashu token without depositing it.
     */
    fun verifyToken(tokenStr: String) {
        val w = wallet ?: run {
            _uiState.value = _uiState.value.copy(errorMessage = "No wallet")
            return
        }
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val value = w.verifyToken(tokenStr.trim())
                _uiState.value = _uiState.value.copy(
                    statusMessage = "Token valid: $value sats",
                    errorMessage = null,
                )
            } catch (e: TollgateException) {
                _uiState.value = _uiState.value.copy(
                    errorMessage = "Invalid token: ${e.detailString()}"
                )
            }
        }
    }

    fun clearError() {
        _uiState.value = _uiState.value.copy(errorMessage = null)
    }

    // --- internals ---

    private fun initWallet(kp: KeyPair) {
        val w = Wallet(defaultRelays, defaultMints)
        w.setIdentity(kp)
        wallet = w
    }

    override fun onCleared() {
        super.onCleared()
        // UniFFI Object handles its own Drop when GC'd.
        wallet = null
    }

    // Extract the human-readable detail from any TollgateException variant.
    // (UniFFI generates the error as a sealed class whose `message` override
    // returns "detail=<text>"; this pulls the bare text instead.)
    private fun TollgateException.detailString(): String = when (this) {
        is TollgateException.InvalidInput -> detail
        is TollgateException.Nostr -> detail
        is TollgateException.Mint -> detail
        is TollgateException.NotReady -> detail
        is TollgateException.Internal -> detail
    }

    companion object {
        private const val TAG = "WalletViewModel"
    }
}
