package com.screenwall.android.data

/** 协议 / 中转层错误码 → 中文文案（与 `web/src/errors.ts` 同一套说法）。 */
private val CODE_COPY: Map<Int, Pair<String, String>> = mapOf(
    4001 to ("登录已失效" to "服务器不认识当前令牌，请退出后重新登录。"),
    4002 to ("协议版本不匹配" to "客户端与被控端版本差距过大，请升级两端。"),
    4003 to ("连接数已满" to "并发会话超过服务器上限，关掉不用的会话再试。"),
    4004 to ("对方关闭了被控" to "被控端关闭了「允许被控」开关。"),
    4005 to ("握手超时" to "网络不稳定，稍后重试。"),
    4006 to ("登录尝试过于频繁" to "请等几分钟再登录。"),
    4100 to ("请求参数有误" to "这次操作的参数不合法。"),
    4101 to ("超过订阅上限" to "同时订阅的画面太多了。"),
    4103 to ("显示器不存在" to "目标显示器已拔掉或编号变了。"),
    4104 to ("编码器不可用" to "被控端没能启动画面编码，重启被控端后再试。"),
    4108 to ("控制已关闭" to "被控端关闭了远程控制功能。"),
    4109 to ("对端不支持该功能" to "被控端版本较旧，升级后可用。"),
    4200 to ("设备不在线" to "目标设备当前离线或已被移除。"),
    4201 to ("会话已断开" to "会话被对端关闭或设备掉线。"),
    4202 to ("画面数据过大" to "单帧超过服务器上限，降低分辨率或画质后再试。"),
    4203 to ("会话号无效" to "这条会话已不存在，重新连接即可。"),
    4204 to ("设备拒绝了会话" to "对方没有允许这次控制请求。"),
    4205 to ("连接过于频繁" to "稍等几秒再试。"),
)

private val HTTP_COPY: Map<String, Pair<String, String>> = mapOf(
    "auth_failed" to ("用户名或密码错误" to "检查账号密码后再试。"),
    "quick_auth_failed" to ("识别码或密码错误" to "检查被控端显示的 9 位识别码与连接密码。"),
    "quick_disabled" to ("设备未开启连接码" to "在被控端打开「连接码连接」后再试。"),
    "control_disabled" to ("对方关闭了被控" to "被控端关闭了「允许被控」开关。"),
    "invalid_token" to ("登录已失效" to "请重新登录。"),
    "rate_limited" to ("尝试过于频繁" to "请等几分钟再试。"),
    "registration_closed" to ("服务器未开放注册" to "请联系服务器管理员开账号。"),
    "device_offline" to ("设备不在线" to "等那台电脑上线后再试。"),
    "internal" to ("服务器内部错误" to "稍后重试；持续出现请查看服务器日志。"),
)

fun describeCode(code: Int, fallback: String = ""): String {
    val hit = CODE_COPY[code]
    if (hit != null) return "${hit.first}：${hit.second}"
    return if (fallback.isNotBlank()) fallback else "操作失败（错误码 $code）"
}

fun describeHttp(error: String, msg: String): String {
    val hit = HTTP_COPY[error]
    if (hit != null) return "${hit.first}：${hit.second}"
    return if (msg.isNotBlank()) msg else error
}

fun describeThrow(error: Throwable): String = when (error) {
    is ApiException -> describeHttp(error.code, error.msg)
    else -> {
        val raw = error.message.orEmpty()
        if (Regex(
                "Failed to connect|timeout|timed out|Unable to resolve host|Network is unreachable|Connection refused",
                RegexOption.IGNORE_CASE,
            ).containsMatchIn(raw)
        ) {
            "连不上服务器：检查服务器地址与网络连接。"
        } else if (raw.isNotBlank()) {
            raw
        } else {
            "操作失败，请重试。"
        }
    }
}
