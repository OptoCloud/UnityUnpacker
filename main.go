package main

import (
	"archive/tar"
	"compress/gzip"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// extractTarGz extracts a .tar.gz file into the specified destination directory.
func extractTarGz(srcFile, destDir string) error {
	// Open the .tar.gz file
	file, err := os.Open(srcFile)
	if err != nil {
		return fmt.Errorf("failed to open file: %w", err)
	}
	defer file.Close()

	// Create a gzip reader
	gzipReader, err := gzip.NewReader(file)
	if err != nil {
		return fmt.Errorf("failed to create gzip reader: %w", err)
	}
	defer gzipReader.Close()

	// Create a tar reader
	tarReader := tar.NewReader(gzipReader)

	// Iterate over the tar archive entries
	for {
		header, err := tarReader.Next()
		if err == io.EOF {
			// End of archive
			break
		}
		if err != nil {
			return fmt.Errorf("failed to read tar archive: %w", err)
		}

		// Compute the path for the extracted file/directory
		targetPath := filepath.Join(destDir, header.Name)

		// Process based on the header type
		switch header.Typeflag {
		case tar.TypeDir:
			break
		case tar.TypeReg:
			// Create the file
			if err := os.MkdirAll(filepath.Dir(targetPath), os.FileMode(0755)); err != nil {
				return fmt.Errorf("failed to create parent directory for file %s: %w", targetPath, err)
			}
			outFile, err := os.Create(targetPath)
			if err != nil {
				return fmt.Errorf("failed to create file %s: %w", targetPath, err)
			}

			// Copy the file contents
			if _, err := io.Copy(outFile, tarReader); err != nil {
				outFile.Close()
				return fmt.Errorf("failed to write file %s: %w", targetPath, err)
			}
			outFile.Close()
			break
		default:
			// Handle other file types as needed (e.g., symbolic links)
			fmt.Printf("Skipping unsupported file type for %s\n", header.Name)
			break
		}
	}

	return nil
}

// reconstructStructure organizes the extracted files into the proper Unity asset structure, and outputs them to a specified target directory.
func reconstructStructure(outputDir string, targetDir string) error {
	entries, err := os.ReadDir(outputDir)
	if err != nil {
		return fmt.Errorf("failed to read output directory: %w", err)
	}

	for _, entry := range entries {
		if entry.IsDir() {
			// Read the "pathname" file in each folder
			metadataPath := filepath.Join(outputDir, entry.Name(), "pathname")
			content, err := os.ReadFile(metadataPath)
			if err != nil {
				return fmt.Errorf("failed to read metadata file %s: %w", metadataPath, err)
			}

			// Construct the target path based on the new targetDir
			newTargetPath := filepath.Join(targetDir, string(content))
			newTargetDir := filepath.Dir(newTargetPath)

			// Ensure the target directory exists
			if err := os.MkdirAll(newTargetDir, os.FileMode(0755)); err != nil {
				return fmt.Errorf("failed to create target directory %s: %w", newTargetDir, err)
			}

			// Check if the "asset" file exists
			sourcePath := filepath.Join(outputDir, entry.Name(), "asset")
			if _, err := os.Stat(sourcePath); err == nil {
				// Copy the asset file to the new target directory
				if err := copyFile(sourcePath, newTargetPath); err != nil {
					return fmt.Errorf("failed to copy asset file %s to %s: %w", sourcePath, newTargetPath, err)
				}

				// Remove the original asset file
				if err := os.Remove(sourcePath); err != nil {
					return fmt.Errorf("failed to delete original asset file %s: %w", sourcePath, err)
				}
			} else if !os.IsNotExist(err) {
				// Return error if it’s not a "file does not exist" error
				return fmt.Errorf("failed to check existence of asset file %s: %w", sourcePath, err)
			}
		}
	}

	return nil
}

// copyFile copies a file from source to destination.
func copyFile(source, destination string) error {
	// Open the source file
	sourceFile, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("failed to open source file %s: %w", source, err)
	}
	defer sourceFile.Close()

	// Create the destination file
	destFile, err := os.Create(destination)
	if err != nil {
		return fmt.Errorf("failed to create destination file %s: %w", destination, err)
	}
	defer destFile.Close()

	// Copy the contents from source to destination
	_, err = io.Copy(destFile, sourceFile)
	if err != nil {
		return fmt.Errorf("failed to copy file from %s to %s: %w", source, destination, err)
	}

	// Ensure the destination file is written to disk
	err = destFile.Sync()
	if err != nil {
		return fmt.Errorf("failed to sync destination file %s: %w", destination, err)
	}

	return nil
}

func main() {
	if len(os.Args) < 2 {
		fmt.Println("Usage: unpack <input_file> [output_folder]")
		os.Exit(1)
	}

	inputFile := os.Args[1]
	var outputDir string

	if len(os.Args) < 3 {
		// Derive output folder name from the input file name
		baseName := filepath.Base(inputFile)
		outputDir = strings.TrimSuffix(baseName, filepath.Ext(baseName))
	} else {
		outputDir = os.Args[2]
	}

	// Ensure the output directory exists
	if err := os.MkdirAll(outputDir, os.FileMode(0755)); err != nil {
		fmt.Printf("Failed to create output directory: %v\n", err)
		os.Exit(1)
	}

	// Use a temporary directory for intermediate files
	tempDir, err := os.MkdirTemp("", "unpack-unitypackage-*")
	if err != nil {
		fmt.Printf("Failed to create temporary directory: %v\n", err)
		os.Exit(1)
	}
	defer func() {
		if err := os.RemoveAll(tempDir); err != nil {
			fmt.Printf("Warning: Failed to delete temporary directory: %v\n", err)
		}
	}()

	// Step 1: Extract the .tar.gz archive into the temporary directory
	if err := extractTarGz(inputFile, tempDir); err != nil {
		fmt.Printf("Failed to extract archive: %v\n", err)
		os.Exit(1)
	}

	// Step 2: Reconstruct the Unity file structure into the output directory
	if err := reconstructStructure(tempDir, outputDir); err != nil {
		fmt.Printf("Failed to reconstruct Unity file structure: %v\n", err)
		os.Exit(1)
	}

	fmt.Printf("Successfully unpacked %s into %s\n", inputFile, outputDir)
}
