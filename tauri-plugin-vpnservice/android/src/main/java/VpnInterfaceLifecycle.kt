package com.plugin.vpnservice

internal class VpnInterfaceLifecycle<T>(
    private val closeInterface: (T) -> Unit,
    initialGeneration: Long = 0,
    private val onGenerationReserved: (Long) -> Unit = {},
) {
    private val retained = linkedMapOf<Long, T>()
    private var highWaterGeneration = initialGeneration
    private var latestGeneration: Long? = null

    @Synchronized
    fun reserveGeneration(generation: Long): Boolean {
        if (generation <= highWaterGeneration) return false
        highWaterGeneration = generation
        latestGeneration = generation
        onGenerationReserved(generation)
        return true
    }

    @Synchronized
    fun addCandidate(generation: Long, candidate: T): Boolean {
        if (generation != latestGeneration || retained.containsKey(generation)) return false
        retained[generation] = candidate
        return true
    }

    @Synchronized
    fun isCurrent(generation: Long): Boolean =
        generation == latestGeneration && retained.containsKey(generation)

    @Synchronized
    fun complete(generation: Long, installed: Boolean = true): Boolean {
        if (!isCurrent(generation)) return false
        if (!installed) return true

        val current = retained.getValue(generation)
        retained.filterKeys { it != generation }.values.forEach(::closeQuietly)
        retained.clear()
        retained[generation] = current
        return true
    }

    @Synchronized
    fun closeAll() {
        retained.values.forEach(::closeQuietly)
        retained.clear()
        latestGeneration = null
    }

    @Synchronized
    fun highWaterGeneration(): Long = highWaterGeneration

    private fun closeQuietly(value: T) {
        try {
            closeInterface(value)
        } catch (_: Exception) {
        }
    }
}
