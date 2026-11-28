package com.screenwall.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.screenwall.android.Prefs
import com.screenwall.android.data.Account
import com.screenwall.android.data.Api
import com.screenwall.android.data.describeThrow
import kotlinx.coroutines.launch

@Composable
fun LoginScreen(
    prefs: Prefs,
    api: Api,
    onLoggedIn: (Account) -> Unit,
) {
    SystemBars(lightIcons = !isSystemInDarkTheme())
    var server by remember { mutableStateOf(prefs.serverUrl) }
    var username by remember { mutableStateOf(prefs.username) }
    var password by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf("") }
    val scope = rememberCoroutineScope()

    fun submit() {
        if (busy) return
        val normalized = Api.normalizeServerUrl(server)
        if (normalized == null) {
            error = "服务器地址无效，例如 http://192.168.1.10:8787"
            return
        }
        if (username.isBlank() || password.isBlank()) {
            error = "请输入用户名和密码"
            return
        }
        busy = true
        error = ""
        scope.launch {
            try {
                val account = api.login(normalized, username.trim(), password)
                prefs.serverUrl = normalized
                prefs.username = account.username
                prefs.saveAccount(account)
                onLoggedIn(account)
            } catch (throwable: Throwable) {
                error = describeThrow(throwable)
            } finally {
                busy = false
            }
        }
    }

    ScreenBackground {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .statusBarsPadding()
                .navigationBarsPadding()
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp),
        ) {
            Spacer(Modifier.height(48.dp))
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                BrandMark(size = 58)
                Column {
                    Text(
                        text = "ScreenWall",
                        style = MaterialTheme.typography.titleLarge,
                        fontSize = 26.sp,
                    )
                    Text(
                        text = "远程控制，随时连回你的电脑",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            Spacer(Modifier.height(32.dp))

            GroupCard {
                LabeledTextField(
                    label = "服务器",
                    value = server,
                    onValueChange = { server = it },
                    placeholder = Prefs.DEFAULT_SERVER,
                    keyboardType = KeyboardType.Uri,
                    imeAction = ImeAction.Next,
                )
                RowDivider()
                LabeledTextField(
                    label = "用户名",
                    value = username,
                    onValueChange = { username = it },
                    placeholder = "账号",
                    imeAction = ImeAction.Next,
                )
                RowDivider()
                LabeledTextField(
                    label = "密码",
                    value = password,
                    onValueChange = { password = it },
                    placeholder = "密码",
                    password = true,
                    keyboardType = KeyboardType.Password,
                    imeAction = ImeAction.Done,
                    onImeAction = { submit() },
                )
            }

            if (error.isNotEmpty()) {
                Text(
                    text = error,
                    color = MaterialTheme.colorScheme.error,
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.padding(top = 12.dp, start = 4.dp, end = 4.dp),
                )
            }

            Spacer(Modifier.height(24.dp))
            PrimaryButton(
                text = if (busy) "正在登录…" else "登录",
                loading = busy,
                onClick = { submit() },
            )
            Spacer(Modifier.height(16.dp))
            Text(
                text = "用桌面客户端同一个账号登录，登录后就能看到在线的电脑。",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 4.dp),
            )
            Spacer(Modifier.height(32.dp))
        }
    }
}
