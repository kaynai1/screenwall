package com.screenwall.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import com.screenwall.android.data.Account
import com.screenwall.android.data.Api
import com.screenwall.android.data.Device
import com.screenwall.android.session.AppSession
import com.screenwall.android.ui.ControlScreen
import com.screenwall.android.ui.DevicesScreen
import com.screenwall.android.ui.HomeScreen
import com.screenwall.android.ui.LoginScreen
import com.screenwall.android.ui.ScreenWallTheme
import com.screenwall.android.ui.SettingsScreen
import kotlinx.coroutines.launch
import okhttp3.OkHttpClient
import java.util.concurrent.TimeUnit

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            ScreenWallTheme {
                AppRoot()
            }
        }
    }
}

private sealed interface Screen {
    data object Login : Screen
    data object Home : Screen
    data object Devices : Screen
    data object Settings : Screen
    data class Control(val device: Device) : Screen
}

@Composable
private fun AppRoot() {
    val context = LocalContext.current
    val prefs = remember { Prefs(context) }
    val http = remember {
        OkHttpClient.Builder()
            .connectTimeout(15, TimeUnit.SECONDS)
            .readTimeout(30, TimeUnit.SECONDS)
            .pingInterval(15, TimeUnit.SECONDS)
            .build()
    }
    val api = remember { Api(http) }
    val scope = rememberCoroutineScope()

    var account by remember { mutableStateOf<Account?>(prefs.loadAccount()) }
    var screen by remember {
        mutableStateOf<Screen>(if (account != null) Screen.Home else Screen.Login)
    }

    val session = remember(account) { account?.let { AppSession(it, api, http) } }
    DisposableEffect(session) {
        onDispose { session?.close() }
    }

    fun logout() {
        val active = session ?: return
        scope.launch {
            active.api.logout(active.account)
            prefs.clearAccount()
            active.close()
            account = null
            screen = Screen.Login
        }
    }

    when (val current = screen) {
        is Screen.Login -> LoginScreen(
            prefs = prefs,
            api = api,
            onLoggedIn = { loggedIn ->
                prefs.saveAccount(loggedIn)
                account = loggedIn
                screen = Screen.Home
            },
        )

        is Screen.Home -> {
            val active = session
            if (active == null) {
                screen = Screen.Login
            } else {
                HomeScreen(
                    session = active,
                    prefs = prefs,
                    onOpenDevice = { device -> screen = Screen.Control(device) },
                    onOpenDevices = { screen = Screen.Devices },
                    onOpenSettings = { screen = Screen.Settings },
                    onQuickConnected = { quickAccount, device ->
                        // 识别码令牌不落盘，只放在这次运行的会话里。
                        account = quickAccount
                        screen = Screen.Control(device)
                    },
                )
            }
        }

        is Screen.Devices -> {
            val active = session
            if (active == null) {
                screen = Screen.Login
            } else {
                DevicesScreen(
                    session = active,
                    onBack = { screen = Screen.Home },
                    onOpenDevice = { device -> screen = Screen.Control(device) },
                    onSettings = { screen = Screen.Settings },
                )
            }
        }

        is Screen.Settings -> {
            val active = session
            if (active == null) {
                screen = Screen.Login
            } else {
                SettingsScreen(
                    session = active,
                    prefs = prefs,
                    onBack = { screen = Screen.Home },
                    onLogout = { logout() },
                    onServerChanged = {
                        active.close()
                        prefs.clearAccount()
                        account = null
                        screen = Screen.Login
                    },
                )
            }
        }

        is Screen.Control -> {
            val active = session
            if (active == null) {
                screen = Screen.Login
            } else {
                ControlScreen(
                    session = active,
                    device = current.device,
                    prefs = prefs,
                    onBack = { screen = Screen.Home },
                )
            }
        }
    }
}
