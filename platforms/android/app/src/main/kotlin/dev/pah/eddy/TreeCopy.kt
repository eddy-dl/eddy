package dev.pah.eddy

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.webkit.MimeTypeMap
import java.io.File

// Puts a staged download's files into a folder picked through the system
// picker (a tree URI the app can't write to by path), keeping sub-folders
// (torrents), then removes the staging copy.
object TreeCopy {
    // Returns null when everything was copied, else what went wrong.
    fun deliver(context: Context, staging: File, treeUri: String): String? {
        return try {
            val tree = Uri.parse(treeUri)
            val root = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
            staging.listFiles()?.forEach { copy(context, it, root) }
            staging.deleteRecursively()
            null
        } catch (e: Exception) {
            // The staging copy stays, so nothing is lost.
            e.message ?: e.toString()
        }
    }

    private fun copy(context: Context, source: File, parent: Uri) {
        val resolver = context.contentResolver
        if (source.isDirectory) {
            val dir = DocumentsContract.createDocument(resolver, parent, DocumentsContract.Document.MIME_TYPE_DIR, source.name)
                ?: error("Couldn't create the folder ${source.name}")
            source.listFiles()?.forEach { copy(context, it, dir) }
            return
        }
        val mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(source.extension.lowercase())
            ?: "application/octet-stream"
        val file = DocumentsContract.createDocument(resolver, parent, mime, source.name)
            ?: error("Couldn't create ${source.name}")
        resolver.openOutputStream(file)?.use { out ->
            source.inputStream().use { it.copyTo(out) }
        } ?: error("Couldn't write ${source.name}")
    }
}
