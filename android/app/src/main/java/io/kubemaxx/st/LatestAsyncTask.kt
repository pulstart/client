package io.kubemaxx.st

import java.util.concurrent.Executor

/** Runs at most one task at a time and only delivers the newest submitted task. */
internal class LatestAsyncTask<T, R>(
    private val executor: Executor,
    private val deliver: (() -> Unit) -> Unit,
    private val work: (T, () -> Boolean) -> R,
    private val complete: (T, R) -> Unit,
) {
    private data class Task<T>(val token: Long, val value: T)

    private val lock = Any()
    private var nextToken = 0L
    private var latest: Task<T>? = null
    private var inFlight = false

    fun submit(value: T) {
        val task = synchronized(lock) {
            nextToken += 1
            latest = Task(nextToken, value)
            if (inFlight) null else latest.also { inFlight = true }
        }
        task?.let(::execute)
    }

    fun cancel() {
        synchronized(lock) {
            nextToken += 1
            latest = null
        }
    }

    private fun execute(task: Task<T>) {
        executor.execute {
            val result = work(task.value) {
                synchronized(lock) { latest?.token == task.token }
            }
            deliver {
                var delivered = false
                val next = synchronized(lock) {
                    if (latest?.token == task.token) {
                        latest = null
                        delivered = true
                    }
                    inFlight = false
                    latest?.also { inFlight = true }
                }
                if (delivered) complete(task.value, result)
                next?.let(::execute)
            }
        }
    }
}
