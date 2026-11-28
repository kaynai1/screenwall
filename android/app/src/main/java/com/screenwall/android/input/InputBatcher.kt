package com.screenwall.android.input

import android.os.Handler
import android.os.Looper
import org.json.JSONArray
import org.json.JSONObject

/** 16ms 一批发送，连续 move 只留最后一个，避免把 WebSocket 打满。 */
class InputBatcher(private val send: (JSONArray) -> Unit) {
    private val handler = Handler(Looper.getMainLooper())
    private val queue = ArrayList<JSONObject>(16)
    private var scheduled = false

    private val flushRunnable = Runnable {
        scheduled = false
        flush()
    }

    fun push(event: JSONObject) {
        if (event.optString("k") == "move" && queue.isNotEmpty()) {
            val last = queue[queue.size - 1]
            if (last.optString("k") == "move") {
                queue[queue.size - 1] = event
                schedule()
                return
            }
        }
        queue.add(event)
        schedule()
    }

    /** 立即发掉队列（点击、按键这类不该等的输入）。 */
    fun flush() {
        handler.removeCallbacks(flushRunnable)
        scheduled = false
        if (queue.isEmpty()) return
        val events = JSONArray()
        for (event in queue) events.put(event)
        queue.clear()
        send(events)
    }

    fun clear() {
        handler.removeCallbacks(flushRunnable)
        scheduled = false
        queue.clear()
    }

    private fun schedule() {
        if (scheduled) return
        scheduled = true
        handler.postDelayed(flushRunnable, 16)
    }
}
