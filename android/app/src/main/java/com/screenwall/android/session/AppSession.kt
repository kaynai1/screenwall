package com.screenwall.android.session

import com.screenwall.android.data.Account
import com.screenwall.android.data.Api
import com.screenwall.android.net.ViewerClient
import okhttp3.OkHttpClient

/** 登录后整条 App 共用的会话：一个到服务器的 WebSocket + 账号接口。 */
class AppSession(
    val account: Account,
    val api: Api,
    val http: OkHttpClient,
) {
    val viewer = ViewerClient(http)

    fun close() {
        viewer.disconnect()
    }
}
