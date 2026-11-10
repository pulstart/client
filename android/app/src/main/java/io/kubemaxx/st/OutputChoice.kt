package io.kubemaxx.st

import org.json.JSONObject

/** `ClientDisplayInfo::output_id` sentinels; real ids are `ServerOutput.id`. */
internal const val OUTPUT_DEFAULT = 0L
internal const val OUTPUT_VIRTUAL = 0xFFFF_FFFFL

internal data class ServerOutput(
    val id: Long,
    val name: String,
    /** Upright pixel size. */
    val width: Int,
    val height: Int,
    val x: Int,
    val y: Int,
    val refreshMilliHz: Int,
    val rotation: Int,
    val primary: Boolean,
    val virtual: Boolean,
) {
    fun label(): String = buildList {
        add(name.ifBlank { "Output $id" })
        add("${width}×$height")
        if (primary) add("primary")
        if (virtual) add("virtual")
    }.joinToString(" · ")
}

internal data class ServerOutputs(
    val outputs: List<ServerOutput>,
    val virtualSupported: Boolean,
) {
    companion object {
        val NONE = ServerOutputs(emptyList(), virtualSupported = false)
    }
}

internal fun shouldShowOutputPicker(outputs: List<ServerOutput>): Boolean = outputs.size > 1

internal fun outputPickerIndex(outputs: List<ServerOutput>, streamedOutputId: Long?): Int =
    if (streamedOutputId == null) -1 else outputs.indexOfFirst { it.id == streamedOutputId }

internal data class RememberedOutput(val id: Long, val name: String)

/** What `output_choices` holds: `serverKey -> {id, name}`. */
internal fun parseOutputChoices(json: String?): Map<String, RememberedOutput> {
    if (json.isNullOrBlank()) return emptyMap()
    return runCatching {
        val root = JSONObject(json)
        buildMap {
            for (key in root.keys()) {
                val entry = root.optJSONObject(key) ?: continue
                val id = entry.optLong("id", -1L)
                if (id !in 1L..0xFFFF_FFFFL) continue
                put(key, RememberedOutput(id, entry.optString("name")))
            }
        }
    }.getOrDefault(emptyMap())
}

internal fun serializeOutputChoices(choices: Map<String, RememberedOutput>): String =
    JSONObject().apply {
        choices.forEach { (key, choice) ->
            put(key, JSONObject().put("id", choice.id).put("name", choice.name))
        }
    }.toString()

/** The output to ask for at connect time; no stored choice means the server's primary. */
internal fun connectOutputId(remembered: RememberedOutput?): Long = remembered?.id ?: OUTPUT_DEFAULT

/**
 * A pick is remembered only once the server confirms it by streaming that
 * output: a switch the server refused, or a fallback after a monitor went
 * away, must not overwrite what the user chose.
 */
internal fun outputChoiceToRemember(
    pendingPick: Long?,
    streamedOutputId: Long,
    outputs: List<ServerOutput>,
): RememberedOutput? {
    if (pendingPick == null || pendingPick != streamedOutputId) return null
    val name = outputs.firstOrNull { it.id == streamedOutputId }?.name.orEmpty()
    return RememberedOutput(streamedOutputId, name)
}

internal sealed interface AuxTarget {
    data class Existing(val id: Long) : AuxTarget
    data object Virtual : AuxTarget
}

/**
 * Output for the external display: the remembered one when it still exists
 * and is not already on the main display, else the first output the main
 * display is not showing, else a new virtual output if the server can make one.
 */
internal fun chooseAuxTarget(
    available: ServerOutputs,
    mainOutputId: Long,
    remembered: RememberedOutput?,
): AuxTarget? {
    val others = available.outputs.filter { it.id != mainOutputId }
    if (remembered != null) {
        if (remembered.id == OUTPUT_VIRTUAL && available.virtualSupported) return AuxTarget.Virtual
        if (others.any { it.id == remembered.id }) return AuxTarget.Existing(remembered.id)
    }
    others.firstOrNull()?.let { return AuxTarget.Existing(it.id) }
    return AuxTarget.Virtual.takeIf { available.virtualSupported }
}

internal fun auxTargetOutputId(target: AuxTarget): Long = when (target) {
    is AuxTarget.Existing -> target.id
    AuxTarget.Virtual -> OUTPUT_VIRTUAL
}

/**
 * Aux sessions are direct-only and never touch the main session: they need a
 * connected, direct main stream and an external display.
 */
internal fun auxSessionWanted(
    externalDisplay: Boolean,
    mainConnected: Boolean,
    mainStream: StreamDescription?,
): Boolean = externalDisplay && mainConnected && mainStream?.directTransport == true
