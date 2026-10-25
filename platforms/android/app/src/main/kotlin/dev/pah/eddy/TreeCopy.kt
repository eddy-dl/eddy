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
    // How a delivery went: what went wrong (null when everything was
    // copied), and the copy of the first file or folder, to open it by.
    class Delivered(val error: String?, val uri: Uri?)

    // `merge`: folders go into ones of the same name already there (more
    // of a finished download's files) rather than beside them.
    fun deliver(context: Context, staging: File, treeUri: String, merge: Boolean = false): Delivered {
        return try {
            val tree = Uri.parse(treeUri)
            val root = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree))
            var first: Uri? = null
            staging.listFiles()?.forEach {
                val copied = copy(context, it, root, merge)
                if (first == null) first = copied
            }
            staging.deleteRecursively()
            Delivered(null, first)
        } catch (e: Exception) {
            // The staging copy stays, so nothing is lost.
            Delivered(e.message ?: e.toString(), null)
        }
    }

    // The folder `name` in `parent` (a document of a tree), if there is one.
    private fun childFolder(context: Context, parent: Uri, name: String): Uri? {
        val children = DocumentsContract.buildChildDocumentsUriUsingTree(parent, DocumentsContract.getDocumentId(parent))
        val columns = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
        )
        context.contentResolver.query(children, columns, null, null, null)?.use { c ->
            while (c.moveToNext()) {
                if (c.getString(1) == name && c.getString(2) == DocumentsContract.Document.MIME_TYPE_DIR) {
                    return DocumentsContract.buildDocumentUriUsingTree(parent, c.getString(0))
                }
            }
        }
        return null
    }

    // Returns the copy's document.
    private fun copy(context: Context, source: File, parent: Uri, merge: Boolean): Uri {
        val resolver = context.contentResolver
        if (source.isDirectory) {
            val dir = (if (merge) childFolder(context, parent, source.name) else null)
                ?: DocumentsContract.createDocument(resolver, parent, DocumentsContract.Document.MIME_TYPE_DIR, source.name)
                ?: error("Couldn't create the folder ${source.name}")
            source.listFiles()?.forEach { copy(context, it, dir, merge) }
            return dir
        }
        val mime = MimeTypeMap.getSingleton().getMimeTypeFromExtension(source.extension.lowercase())
            ?: "application/octet-stream"
        val file = DocumentsContract.createDocument(resolver, parent, mime, source.name)
            ?: error("Couldn't create ${source.name}")
        resolver.openOutputStream(file)?.use { out ->
            source.inputStream().use { it.copyTo(out) }
        } ?: error("Couldn't write ${source.name}")
        return file
    }
}
