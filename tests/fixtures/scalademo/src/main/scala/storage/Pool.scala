package storage

/** Exported helper. */
def openPool(url: String, max: Int): String = s"$url:$max"

private def hiddenPool(url: String): String = url
