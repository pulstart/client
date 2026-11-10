package io.kubemaxx.st

import android.content.Context
import android.graphics.Color
import android.view.View
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView

/**
 * A short list of outputs. Rows are plain taps rather than a spinner: a pick
 * restarts the server encoder, so it must only ever come from the user, never
 * from a selection callback fired by a programmatic update.
 */
internal class OutputPickerView(
    context: Context,
    private val accent: Int,
    private val textColor: Int,
    private val dp: (Int) -> Int,
    private val onPick: (Int) -> Unit,
) : LinearLayout(context) {
    private var labels = emptyList<String>()
    private var selected = NONE
    private var pending = NONE

    init {
        orientation = VERTICAL
    }

    /** `selected` follows what the server streams; `pending` is a pick awaiting the server's answer. */
    fun setEntries(labels: List<String>, selected: Int, pending: Int) {
        if (labels == this.labels && selected == this.selected && pending == this.pending) return
        this.labels = labels
        this.selected = selected
        this.pending = pending
        removeAllViews()
        labels.forEachIndexed { index, label -> addView(row(index, label)) }
    }

    private fun row(index: Int, label: String) = TextView(context).apply {
        val isSelected = index == selected
        val isPending = index == pending && !isSelected
        text = if (isPending) "$label …" else label
        textSize = 14f
        setTextColor(if (isSelected) accent else textColor)
        setPadding(dp(10), dp(9), dp(10), dp(9))
        isClickable = true
        isFocusable = true
        contentDescription = label
        if (isSelected) setBackgroundColor(Color.argb(40, 90, 200, 250))
        setOnClickListener { onPick(index) }
        layoutParams = LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        visibility = View.VISIBLE
    }

    private companion object {
        const val NONE = -2
    }
}
