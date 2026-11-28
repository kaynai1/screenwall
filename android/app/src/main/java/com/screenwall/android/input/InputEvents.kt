package com.screenwall.android.input

import org.json.JSONArray
import org.json.JSONObject
import kotlin.math.roundToInt

/**
 * 触摸 / 键盘 → 协议 `input` 事件。
 *
 * 字段与 `web/src/input.ts`、`crates/proto/src/messages.rs::InputEvent` 一致。
 */
object InputEvents {
    fun moveAbs(x: Float, y: Float): JSONObject =
        JSONObject().put("k", "move").put("abs", pair(x, y))

    fun moveRel(dx: Int, dy: Int): JSONObject =
        JSONObject().put("k", "move").put("rel", JSONArray().put(dx).put(dy))

    fun button(name: String, down: Boolean, x: Float? = null, y: Float? = null): JSONObject {
        val event = JSONObject().put("k", "button").put("button", name).put("down", down)
        if (x != null && y != null) event.put("abs", pair(x, y))
        return event
    }

    fun wheel(dx: Int, dy: Int): JSONObject =
        JSONObject().put("k", "wheel").put("delta", JSONArray().put(dx).put(dy))

    fun key(scan: Int, down: Boolean, ext: Boolean = false): JSONObject =
        JSONObject().put("k", "key").put("scan", scan).put("down", down).put("ext", ext)

    fun text(value: String): JSONObject = JSONObject().put("k", "text").put("text", value)

    fun releaseAll(): JSONObject = JSONObject().put("k", "releaseAll")

    /** 协议用 0..1 归一化坐标，保留 4 位小数。 */
    private fun pair(x: Float, y: Float): JSONArray {
        fun clamp(value: Float): Double =
            ((value.coerceIn(0f, 1f) * 10000f).roundToInt() / 10000.0)
        return JSONArray().put(clamp(x)).put(clamp(y))
    }
}
